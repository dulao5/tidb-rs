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

// ---------------------------------------------------------------- D08.12 batch 1
//
// Expression tree utilities of util.go whose dependencies are ported (D08.12 spec §1).

use std::collections::{BTreeSet, HashMap, HashSet};

use tidb_base::gofmt::strconv_quote;
use tidb_base::gostd::strconv::{atoi, format_float};
use tidb_types::datum::{Datum, KindInt64, KindNull, KindUint64};
use tidb_types::etc::{FieldType, is_type_varchar};

use crate::column::CorrelatedColumn;
use crate::constant::Constant;
use crate::expression::{
    EvalState, compose_cnf_condition, compose_dnf_condition, flatten_cnf_conditions,
    flatten_dnf_conditions, split_cnf_items, wrap_with_is_true,
};
use crate::function_traits::unFoldableFunctions;
use crate::scalar_function::ScalarFunction;
use crate::schema::expr_from_schema;

/// Filter the input expressions, append the results to result.
// go: Filter L1
pub fn filter(
    mut result: Vec<ExprRef>,
    input: &[ExprRef],
    filter: impl Fn(&ExprRef) -> bool,
) -> Vec<ExprRef> {
    for e in input {
        if filter(e) {
            result.push(Arc::clone(e));
        }
    }
    result
}

/// FilterOutInPlace do the filtering out in place.
/// The remained are the ones who doesn't match the filter, storing in the original slice.
/// The filteredOut are the ones match the filter, storing in a new slice.
// go: FilterOutInPlace L2 (returns the remained vector instead of re-slicing the input)
pub fn filter_out_in_place(
    mut input: Vec<ExprRef>,
    filter: impl Fn(&ExprRef) -> bool,
) -> (Vec<ExprRef>, Vec<ExprRef>) {
    let mut filtered_out = Vec::new();
    for i in (0..input.len()).rev() {
        if filter(&input[i]) {
            filtered_out.push(input.remove(i));
        }
    }
    (input, filtered_out)
}

/// ExtractDependentColumns extracts all dependent columns from a virtual column.
// go: ExtractDependentColumns L1
pub fn extract_dependent_columns(expr: &ExprRef) -> Vec<Arc<Column>> {
    // Pre-allocate a slice to reduce allocation, 8 doesn't have special meaning.
    let mut result = Vec::with_capacity(8);
    extract_dependent_columns_impl(&mut result, expr);
    result
}

// go: extractDependentColumns L2 (appends in place; the column is copied into an Arc)
fn extract_dependent_columns_impl(result: &mut Vec<Arc<Column>>, expr: &ExprRef) {
    match &**expr {
        Expr::Column(v) => {
            result.push(Arc::new(v.clone()));
            if let Some(ve) = &v.virtual_expr {
                extract_dependent_columns_impl(result, ve);
            }
        }
        Expr::ScalarFunction(v) => {
            for arg in v.get_args() {
                extract_dependent_columns_impl(result, arg);
            }
        }
        _ => {}
    }
}

/// ExtractAllColumnsFromExpressionsInUsedSlices is the same as ExtractColumns. but it can
/// reuse the memory.
// go: ExtractAllColumnsFromExpressionsInUsedSlices L1
pub fn extract_all_columns_from_expressions_in_used_slices(
    mut reuse: Vec<Arc<Column>>,
    filter: Option<&dyn Fn(&Column) -> bool>,
    exprs: &[ExprRef],
) -> Vec<Arc<Column>> {
    if exprs.is_empty() {
        return Vec::new();
    }
    for expr in exprs {
        extract_columns_slices(&mut reuse, expr, filter);
    }
    // slices.SortFunc is not stable; columns with one UniqueID are merged by CompactFunc
    // below, which keeps the first of each run (any of them: they compare equal).
    reuse.sort_by_key(|c| c.unique_id);
    reuse.dedup_by(|b, a| a.unique_id == b.unique_id);
    reuse
}

// go: extractColumnsSlices L2 (appends in place; the column is copied into an Arc)
fn extract_columns_slices(
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
                extract_columns_slices(result, arg, filter);
            }
        }
        _ => {}
    }
}

/// ExtractCorColumns extracts correlated column from given expression.
// go: ExtractCorColumns L2 (the correlated column is copied into an Arc; it keeps its slot)
pub fn extract_cor_columns(expr: &ExprRef) -> Vec<Arc<CorrelatedColumn>> {
    let mut cols = Vec::new();
    match &**expr {
        Expr::CorrelatedColumn(v) => return vec![Arc::new(v.clone())],
        Expr::ScalarFunction(v) => {
            for arg in v.get_args() {
                cols.extend(extract_cor_columns(arg));
            }
        }
        _ => {}
    }
    cols
}

/// ExtractColumnsFromExpressions is a more efficient version of ExtractColumns for batch
/// operation. filter can be nil, or a function to filter the result column.
// go: ExtractColumnsFromExpressions L1
pub fn extract_columns_from_expressions(
    mut result: Vec<Arc<Column>>,
    exprs: &[ExprRef],
    filter: Option<&dyn Fn(&Column) -> bool>,
) -> Vec<Arc<Column>> {
    for expr in exprs {
        extract_columns_impl(&mut result, expr, filter);
    }
    result
}

/// ExtractEquivalenceColumns detects the equivalence from CNF exprs.
// go: ExtractEquivalenceColumns L1
pub fn extract_equivalence_columns(
    mut result: Vec<Vec<ExprRef>>,
    exprs: &[ExprRef],
) -> Vec<Vec<ExprRef>> {
    // exprs are CNF expressions, EQ condition only make sense in the top level of every expr.
    for expr in exprs {
        extract_equivalence_columns_impl(&mut result, expr);
    }
    result
}

/// FindUpperBound looks for column < constant or column <= constant and returns both the
/// column and constant. It return nil, 0 if the expression is not of this form.
// go: FindUpperBound L1
pub fn find_upper_bound(expr: &Expr) -> (Option<Arc<Column>>, i64) {
    if let Expr::ScalarFunction(scalar_function) = expr {
        let args = scalar_function.get_args();
        if args.len() == 2
            && let (Expr::Column(col), Expr::Constant(constant)) = (&*args[0], &*args[1])
            && (scalar_function.func_name.l == ast::LT || scalar_function.func_name.l == ast::LE)
            && constant.value.kind() == KindInt64
        {
            let value = constant.value.get_int64();
            if scalar_function.func_name.l == ast::LT {
                // Go: value - 1 wraps on MinInt64.
                return (Some(Arc::new(col.clone())), value.wrapping_sub(1));
            }
            return (Some(Arc::new(col.clone())), value);
        }
    }
    (None, 0)
}

// go: extractEquivalenceColumns L2 (appends in place)
fn extract_equivalence_columns_impl(result: &mut Vec<Vec<ExprRef>>, expr: &ExprRef) {
    if let Expr::ScalarFunction(v) = &**expr {
        // a==b, a<=>b, the latter one is evaluated to true when a,b are both null.
        let l = v.func_name.l.as_str();
        let args = v.get_args();
        let pairs = |result: &mut Vec<Vec<ExprRef>>, a: &ExprRef, b: &ExprRef| {
            if matches!(&**a, Expr::Column(_)) && matches!(&**b, Expr::Column(_)) {
                result.push(vec![Arc::clone(a), Arc::clone(b)]);
            }
            if matches!(&**a, Expr::Column(_)) && matches!(&**b, Expr::ScalarFunction(_)) {
                result.push(vec![Arc::clone(a), Arc::clone(b)]);
            }
            if matches!(&**b, Expr::Column(_)) && matches!(&**a, Expr::ScalarFunction(_)) {
                result.push(vec![Arc::clone(b), Arc::clone(a)]);
            }
        };
        if l == ast::EQ || l == ast::NullEQ {
            if args.len() == 2 {
                pairs(result, &args[0], &args[1]);
            }
            return;
        }
        if l == ast::In {
            // only `col in (only 1 element)`, can we build an equivalence here.
            if args.len() - 1 == 1 {
                pairs(result, &args[0], &args[1]);
            }
        }
        // For Non-EQ function, we don't have to traverse down.
        // eg: (a=b or c=d) doesn't make any definitely equivalence assertion.
    }
}

/// extractColumnsAndCorColumns extracts columns and correlated columns from `expr` and
/// append them to `result`.
// go: extractColumnsAndCorColumns L2 (appends in place; the column is copied into an Arc)
fn extract_columns_and_cor_columns(result: &mut Vec<Arc<Column>>, expr: &ExprRef) {
    match &**expr {
        Expr::Column(v) => result.push(Arc::new(v.clone())),
        Expr::CorrelatedColumn(v) => result.push(Arc::new(v.column.clone())),
        Expr::ScalarFunction(v) => {
            for arg in v.get_args() {
                extract_columns_and_cor_columns(result, arg);
            }
        }
        _ => {}
    }
}

/// ExtractConstantEqColumnsOrScalar detects the constant equal relationship from CNF exprs.
// go: ExtractConstantEqColumnsOrScalar L1
pub fn extract_constant_eq_columns_or_scalar(
    ctx: &dyn BuildContext,
    mut result: Vec<ExprRef>,
    exprs: &[ExprRef],
) -> Vec<ExprRef> {
    // exprs are CNF expressions, EQ condition only make sense in the top level of every expr.
    for expr in exprs {
        extract_constant_eq_columns_or_scalar_impl(ctx, &mut result, expr);
    }
    result
}

// go: extractConstantEqColumnsOrScalar L2 (appends in place)
fn extract_constant_eq_columns_or_scalar_impl(
    ctx: &dyn BuildContext,
    result: &mut Vec<ExprRef>,
    expr: &ExprRef,
) {
    let Expr::ScalarFunction(v) = &**expr else {
        return;
    };
    let l = v.func_name.l.as_str();
    let args = v.get_args();
    if l == ast::EQ || l == ast::NullEQ {
        if args.len() == 2 {
            let is_col = |e: &ExprRef| matches!(&**e, Expr::Column(_));
            let is_sf = |e: &ExprRef| matches!(&**e, Expr::ScalarFunction(_));
            let is_con = |e: &ExprRef| matches!(&**e, Expr::Constant(_));
            let is_cor = |e: &ExprRef| matches!(&**e, Expr::CorrelatedColumn(_));
            let (a, b) = (&args[0], &args[1]);
            if is_col(a) && is_con(b) {
                result.push(Arc::clone(a));
            }
            if is_col(b) && is_con(a) {
                result.push(Arc::clone(b));
            }
            // take the correlated column as constant here.
            if is_col(a) && is_cor(b) {
                result.push(Arc::clone(a));
            }
            if is_col(b) && is_cor(a) {
                result.push(Arc::clone(b));
            }
            if is_sf(a) && is_con(b) {
                result.push(Arc::clone(a));
            }
            if is_sf(b) && is_con(a) {
                result.push(Arc::clone(b));
            }
            // take the correlated column as constant here.
            if is_sf(a) && is_cor(b) {
                result.push(Arc::clone(a));
            }
            if is_sf(b) && is_cor(a) {
                result.push(Arc::clone(b));
            }
        }
        return;
    }
    if l == ast::In {
        let mut all_args_is_const = true;
        // only `col in (all same const)`, can col be the constant column.
        // eg: a in (1, "1") does, while a in (1, '2') doesn't.
        let guard = &args[1];
        for (i, v) in args[1..].iter().enumerate() {
            if !matches!(&**v, Expr::Constant(_)) {
                all_args_is_const = false;
                break;
            }
            if i == 0 {
                continue;
            }
            if !guard.equal(&*ctx.get_eval_ctx(), v) {
                all_args_is_const = false;
                break;
            }
        }
        if all_args_is_const && matches!(&*args[0], Expr::Column(_) | Expr::ScalarFunction(_)) {
            result.push(Arc::clone(&args[0]));
        }
    }
    // For Non-EQ function, we don't have to traverse down.
}

/// ExtractColumnsAndCorColumnsFromExpressions extracts columns and correlated columns from
/// expressions and append them to `result`.
// go: ExtractColumnsAndCorColumnsFromExpressions L1
pub fn extract_columns_and_cor_columns_from_expressions(
    mut result: Vec<Arc<Column>>,
    list: &[ExprRef],
) -> Vec<Arc<Column>> {
    for expr in list {
        extract_columns_and_cor_columns(&mut result, expr);
    }
    result
}

/// ExtractColumnSet extracts the different values of `UniqueId` for columns in expressions.
/// `intset.FastIntSet` (D00.3) is not ported; an ordered set of the same values stands for
/// it (its `Ordered()` iterates in ascending order too).
// go: ExtractColumnSet L2 (BTreeSet for intset.FastIntSet, D08.12 spec §4)
pub fn extract_column_set(exprs: &[ExprRef]) -> BTreeSet<i64> {
    let mut set = BTreeSet::new();
    for expr in exprs {
        extract_column_set_impl(expr, &mut set);
    }
    set
}

// Go converts UniqueID to int (a no-op on 64-bit).
// go: extractColumnSet L2 (BTreeSet for intset.FastIntSet, D08.12 spec §4)
fn extract_column_set_impl(expr: &ExprRef, set: &mut BTreeSet<i64>) {
    match &**expr {
        Expr::Column(v) => {
            set.insert(v.unique_id);
        }
        Expr::ScalarFunction(v) => {
            for arg in v.get_args() {
                extract_column_set_impl(arg, set);
            }
        }
        _ => {}
    }
}

/// `oppositeOp`: the operator of the negated condition.
// glue: Go's oppositeOp map (util.go) as a match over its keys
fn opposite_op(op: &str) -> &'static str {
    match op {
        ast::LT => ast::GE,
        ast::GE => ast::LT,
        ast::GT => ast::LE,
        ast::LE => ast::GT,
        ast::EQ => ast::NE,
        ast::NE => ast::EQ,
        ast::LogicOr => ast::LogicAnd,
        ast::LogicAnd => ast::LogicOr,
        // Go: a missing key gives "", which NewFunctionInternal rejects (nil Expression).
        _ => "",
    }
}

// go: pushNotAcrossArgs L1
fn push_not_across_args(
    ctx: &dyn BuildContext,
    exprs: &[ExprRef],
    not: bool,
) -> (Vec<Option<ExprRef>>, bool) {
    let mut new_exprs = Vec::with_capacity(exprs.len());
    let mut flag = false;
    for expr in exprs {
        let (new_expr, changed) = push_not_across_expr(ctx, Some(Arc::clone(expr)), not);
        flag = changed || flag;
        new_exprs.push(new_expr);
    }
    (new_exprs, flag)
}

/// `NewFunctionInternal` over arguments that may be Go nil Expressions (a nested
/// `NewFunctionInternal` failed): Go passes them on and newFunctionImpl panics on them.
// glue: nil arguments of NewFunctionInternal
fn new_function_internal_opt(
    ctx: &dyn BuildContext,
    name: &str,
    ret_type: FieldType,
    args: Vec<Option<ExprRef>>,
) -> Option<ExprRef> {
    let args: Option<Vec<ExprRef>> = args.into_iter().collect();
    let Some(args) = args else {
        panic!("runtime error: invalid memory address or nil pointer dereference");
    };
    new_function_internal(ctx, name, ret_type, args)
}

/// pushNotAcrossExpr try to eliminate the NOT expr in expression tree.
/// Input `not` indicates whether there's a `NOT` be pushed down.
/// Output `changed` indicates whether the output expression differs from the
/// input `expr` because of the pushed-down-not. `None` is Go's nil Expression (a
/// `NewFunctionInternal` that failed).
// go: pushNotAcrossExpr L1
fn push_not_across_expr(
    ctx: &dyn BuildContext,
    expr: Option<ExprRef>,
    not: bool,
) -> (Option<ExprRef>, bool) {
    let Some(e) = expr else {
        // Go: a nil Expression is not a *ScalarFunction.
        if not {
            return (
                new_function_internal_opt(
                    ctx,
                    ast::UnaryNot,
                    new_field_type(mysql::TypeTiny),
                    vec![None],
                ),
                true,
            );
        }
        return (None, false);
    };
    if let Expr::ScalarFunction(f) = &*e {
        let l = f.func_name.l.as_str();
        match l {
            ast::UnaryNot => {
                let Ok(child) = wrap_with_is_true(ctx, true, Arc::clone(&f.get_args()[0]), true)
                else {
                    return (Some(e), false);
                };
                let (child_expr, changed) = push_not_across_expr(ctx, Some(child), !not);
                if !changed && !not {
                    return (Some(e), false);
                }
                return (child_expr, true);
            }
            ast::LT | ast::GE | ast::GT | ast::LE | ast::EQ | ast::NE => {
                if not {
                    return (
                        new_function_internal(
                            ctx,
                            opposite_op(l),
                            f.get_type().clone(),
                            f.get_args().to_vec(),
                        ),
                        true,
                    );
                }
                let (new_args, changed) = push_not_across_args(ctx, f.get_args(), false);
                if !changed {
                    return (Some(e), false);
                }
                return (
                    new_function_internal_opt(ctx, l, f.get_type().clone(), new_args),
                    true,
                );
            }
            ast::LogicAnd | ast::LogicOr => {
                let mut func_name = l;
                let new_args;
                let changed;
                if not {
                    new_args = push_not_across_args(ctx, f.get_args(), true).0;
                    func_name = opposite_op(l);
                    changed = true;
                } else {
                    (new_args, changed) = push_not_across_args(ctx, f.get_args(), false);
                }
                if !changed {
                    return (Some(e), false);
                }
                return (
                    new_function_internal_opt(ctx, func_name, f.get_type().clone(), new_args),
                    true,
                );
            }
            _ => {}
        }
    }
    if not {
        return (
            new_function_internal(ctx, ast::UnaryNot, new_field_type(mysql::TypeTiny), vec![e]),
            true,
        );
    }
    (Some(e), false)
}

/// GetExprInsideIsTruth get the expression inside the `istrue_with_null` and `istrue`.
/// This is useful when handling expressions from "not" or "!", because we might wrap
/// `istrue_with_null` or `istrue` when handling them. See pushNotAcrossExpr() and
/// wrapWithIsTrue() for details.
// go: GetExprInsideIsTruth L1
pub fn get_expr_inside_is_truth(expr: &ExprRef) -> ExprRef {
    if let Expr::ScalarFunction(f) = &**expr {
        return match f.func_name.l.as_str() {
            ast::IsTruthWithNull | ast::IsTruthWithoutNull => {
                get_expr_inside_is_truth(&f.get_args()[0])
            }
            _ => Arc::clone(expr),
        };
    }
    Arc::clone(expr)
}

/// PushDownNot pushes the `not` function down to the expression's arguments. `None` is Go's
/// nil Expression (a `NewFunctionInternal` that failed).
// go: PushDownNot L1
pub fn push_down_not(ctx: &dyn BuildContext, expr: &ExprRef) -> Option<ExprRef> {
    push_not_across_expr(ctx, Some(Arc::clone(expr)), false).0
}

// go: noPrecisionLossCastCompatible L1
fn no_precision_loss_cast_compatible(cast: &FieldType, arg_col: &FieldType) -> bool {
    // now only consider varchar type and integer.
    if !(is_type_varchar(cast.get_type()) && is_type_varchar(arg_col.get_type()))
        && !(mysql::is_integer_type(cast.get_type()) && mysql::is_integer_type(arg_col.get_type()))
    {
        // varchar type and integer on the storage layer is quite same, while the char type
        // has its padding suffix.
        return false;
    }
    if is_type_varchar(cast.get_type()) {
        // cast varchar function only bear the flen extension.
        if cast.get_flen() < arg_col.get_flen() {
            return false;
        }
        if !tidb_types::collate::compatible_collate(cast.get_collate(), arg_col.get_collate()) {
            return false;
        }
    } else {
        // For integers, we should ignore the potential display length represented by flen,
        // using the default flen of the type.
        let (cast_flen, _) = mysql::get_default_field_length_and_decimal(cast.get_type());
        let (origin_flen, _) = mysql::get_default_field_length_and_decimal(arg_col.get_type());
        // cast integer function only bear the flen extension and signed symbol unchanged.
        if cast_flen < origin_flen {
            return false;
        }
        if mysql::has_unsigned_flag(cast.get_flag()) != mysql::has_unsigned_flag(arg_col.get_flag())
        {
            return false;
        }
    }
    true
}

// go: unwrapCast L1
fn unwrap_cast(
    sctx: &dyn BuildContext,
    parent_f: &ScalarFunction,
    parent: &ExprRef,
    cast_offset: usize,
) -> (Option<ExprRef>, bool) {
    let (_, collation) = parent_f.charset_and_collation();
    let Expr::ScalarFunction(cast) = &*parent_f.get_args()[cast_offset] else {
        return (Some(Arc::clone(parent)), false);
    };
    if cast.func_name.l != ast::Cast {
        return (Some(Arc::clone(parent)), false);
    }
    // eg: if (cast(A) EQ const) with incompatible collation, even if cast is eliminated, the
    // condition still can not be used to build range.
    if cast.ret_type().eval_type() == tidb_parser::types::ETString
        && !tidb_types::collate::compatible_collate(cast.ret_type().get_collate(), collation)
    {
        return (Some(Arc::clone(parent)), false);
    }
    // 1-castOffset should be constant
    if !matches!(&*parent_f.get_args()[1 - cast_offset], Expr::Constant(_)) {
        return (Some(Arc::clone(parent)), false);
    }
    // the direct args of cast function should be column.
    let Expr::Column(c) = &*cast.get_args()[0] else {
        return (Some(Arc::clone(parent)), false);
    };
    // current only consider varchar and integer
    if !no_precision_loss_cast_compatible(cast.ret_type(), &c.ret_type) {
        return (Some(Arc::clone(parent)), false);
    }
    let c = Arc::clone(&cast.get_args()[0]);
    // the column is covered by indexes, deconstructing it out.
    let args = if cast_offset == 0 {
        vec![c, Arc::clone(&parent_f.get_args()[1])]
    } else {
        vec![Arc::clone(&parent_f.get_args()[0]), c]
    };
    (
        new_function_internal(
            sctx,
            &parent_f.func_name.l,
            parent_f.ret_type().clone(),
            args,
        ),
        true,
    )
}

/// eliminateCastFunction will detect the original arg before and the cast type after, once
/// upon there is no precision loss between them, current cast wrapper can be eliminated. For
/// string type, collation is also taken into consideration. (mainly used to build range or
/// point). `None` is Go's nil Expression (a failed `NewFunctionInternal` /
/// `ComposeDNFCondition`).
// go: eliminateCastFunction L1
fn eliminate_cast_function(sctx: &dyn BuildContext, expr: &ExprRef) -> (Option<ExprRef>, bool) {
    let Expr::ScalarFunction(f) = &**expr else {
        return (Some(Arc::clone(expr)), false);
    };
    let (_, collation) = expr.charset_and_collation();
    match f.func_name.l.as_str() {
        ast::LogicOr | ast::LogicAnd => {
            let or = f.func_name.l == ast::LogicOr;
            let items = if or {
                flatten_dnf_conditions(f)
            } else {
                flatten_cnf_conditions(f)
            };
            let mut rm_cast = false;
            let mut rm_cast_items = Vec::with_capacity(items.len());
            for item in &items {
                let (new_expr, cur_downcast) = eliminate_cast_function(sctx, item);
                rm_cast_items.push(new_expr);
                if cur_downcast {
                    rm_cast = true;
                }
            }
            if rm_cast {
                // compose the new DNF / CNF expression; a nil item panics in Go's
                // composeConditionWithBinaryOp (NewFunctionInternal on nil).
                let items: Vec<ExprRef> = rm_cast_items
                    .into_iter()
                    .map(|e| {
                        e.unwrap_or_else(|| {
                            panic!(
                                "runtime error: invalid memory address or nil pointer dereference"
                            )
                        })
                    })
                    .collect();
                return if or {
                    (compose_dnf_condition(sctx, &items), true)
                } else {
                    (compose_cnf_condition(sctx, &items), true)
                };
            }
            (Some(Arc::clone(expr)), false)
        }
        ast::EQ | ast::NullEQ | ast::LE | ast::GE | ast::LT | ast::GT => {
            // for case: eq(cast(test.t2.a, varchar(100), "aaaaa"), once t2.a is covered by
            // index or pk, try deconstructing it out.
            let (new_f, ok) = unwrap_cast(sctx, f, expr, 0);
            if ok {
                return (new_f, true);
            }
            // for case: eq("aaaaa"， cast(test.t2.a, varchar(100)), once t2.a is covered by
            // index or pk, try deconstructing it out.
            let (new_f, ok) = unwrap_cast(sctx, f, expr, 1);
            if ok {
                return (new_f, true);
            }
            (Some(Arc::clone(expr)), false)
        }
        ast::In => {
            // case for: cast(a<int> as bigint) in (1,2,3), we could deconstruct column 'a out
            // directly.
            let Expr::ScalarFunction(cast) = &*f.get_args()[0] else {
                return (Some(Arc::clone(expr)), false);
            };
            if cast.func_name.l != ast::Cast {
                return (Some(Arc::clone(expr)), false);
            }
            // eg: if (cast(A) IN {const}) with incompatible collation, even if cast is
            // eliminated, the condition still can not be used to build range.
            if cast.ret_type().eval_type() == tidb_parser::types::ETString
                && !tidb_types::collate::compatible_collate(
                    cast.ret_type().get_collate(),
                    collation,
                )
            {
                return (Some(Arc::clone(expr)), false);
            }
            for arg in &f.get_args()[1..] {
                if !matches!(&**arg, Expr::Constant(_)) {
                    return (Some(Arc::clone(expr)), false);
                }
            }
            // the direct args of cast function should be column.
            let Expr::Column(c) = &*cast.get_args()[0] else {
                return (Some(Arc::clone(expr)), false);
            };
            // current only consider varchar and integer
            if !no_precision_loss_cast_compatible(cast.ret_type(), &c.ret_type) {
                return (Some(Arc::clone(expr)), false);
            }
            let mut new_args = vec![Arc::clone(&cast.get_args()[0])];
            new_args.extend(f.get_args()[1..].iter().cloned());
            (
                new_function_internal(sctx, &f.func_name.l, f.ret_type().clone(), new_args),
                true,
            )
        }
        _ => (Some(Arc::clone(expr)), false),
    }
}

/// EliminateNoPrecisionLossCast remove the redundant cast function for range build
/// convenience.
/// 1: deeper cast embedded in other complicated function will not be considered.
/// 2: cast args should be one for original base column and one for constant.
/// 3: some collation compatibility and precision loss will be considered when remove this
///    cast func.
// go: EliminateNoPrecisionLossCast L1
pub fn eliminate_no_precision_loss_cast(
    sctx: &dyn BuildContext,
    expr: &ExprRef,
) -> Option<ExprRef> {
    eliminate_cast_function(sctx, expr).0
}

/// ContainOuterNot checks if there is an outer `not`.
// go: ContainOuterNot L1
pub fn contain_outer_not(expr: &Expr) -> bool {
    contain_outer_not_impl(expr, false)
}

/// containOuterNot checks if there is an outer `not`.
/// Input `not` means whether there is `not` outside `expr`
///
/// eg.
///
///   not(0+(t.a == 1 and t.b == 2)) returns true
///   not(t.a) and not(t.b) returns false
// go: containOuterNot L1
fn contain_outer_not_impl(expr: &Expr, not: bool) -> bool {
    if let Expr::ScalarFunction(f) = expr {
        return match f.func_name.l.as_str() {
            ast::UnaryNot => contain_outer_not_impl(&f.get_args()[0], true),
            ast::IsTruthWithNull | ast::IsNull => contain_outer_not_impl(&f.get_args()[0], not),
            _ => {
                if not {
                    return true;
                }
                let mut has_not = false;
                for expr in f.get_args() {
                    has_not = has_not || contain_outer_not_impl(expr, not);
                    if has_not {
                        return has_not;
                    }
                }
                has_not
            }
        };
    }
    false
}

/// Contains tests if `exprs` contains `e`.
// go: Contains L1
pub fn contains(ectx: &dyn EvalContext, exprs: &[ExprRef], e: &ExprRef) -> bool {
    exprs
        .iter()
        .any(|expr| Arc::ptr_eq(e, expr) || expr.equal(ectx, e))
}

/// ExtractFiltersFromDNFs checks whether the cond is DNF. If so, it will get the extracted
/// part and the remained part. The original DNF will be replaced by the remained part or just
/// be deleted if remained part is nil. And the extracted part will be appended to the end of
/// the original slice.
// go: ExtractFiltersFromDNFs L1
pub fn extract_filters_from_dnfs(
    ctx: &dyn BuildContext,
    mut conditions: Vec<ExprRef>,
) -> Vec<ExprRef> {
    let mut all_extracted = Vec::new();
    for i in (0..conditions.len()).rev() {
        if let Expr::ScalarFunction(sf) = &*conditions[i]
            && sf.func_name.l == ast::LogicOr
        {
            let (extracted, remained) = extract_filters_from_dnf(ctx, sf, &conditions[i]);
            all_extracted.extend(extracted);
            match remained {
                None => {
                    conditions.remove(i);
                }
                Some(r) => conditions[i] = r,
            }
        }
    }
    conditions.extend(all_extracted);
    conditions
}

/// extractFiltersFromDNF extracts the same condition that occurs in every DNF item and remove
/// them from dnf leaves.
///
/// Go collects the extracted items by ranging over a map, so their order is random; here
/// they come in the order of their first appearance in the first DNF item (D08.12 spec §4).
// go: extractFiltersFromDNF L2 (deterministic order of the extracted items, M-MAPORDER)
pub fn extract_filters_from_dnf(
    ctx: &dyn BuildContext,
    dnf_func: &ScalarFunction,
    dnf_expr: &ExprRef,
) -> (Vec<ExprRef>, Option<ExprRef>) {
    let dnf_items = flatten_dnf_conditions(dnf_func);
    let mut code_map: HashMap<Vec<u8>, usize> = HashMap::new();
    let mut hashcode2_expr: HashMap<Vec<u8>, ExprRef> = HashMap::new();
    let mut first_order: Vec<Vec<u8>> = Vec::new();
    for (i, dnf_item) in dnf_items.iter().enumerate() {
        let mut inner_map: HashSet<Vec<u8>> = HashSet::new();
        let cnf_items = split_cnf_items(dnf_item);
        for cnf_item in &cnf_items {
            let code = cnf_item.hash_code().to_vec();
            if i == 0 {
                if !hashcode2_expr.contains_key(&code) {
                    first_order.push(code.clone());
                }
                code_map.insert(code.clone(), 1);
                hashcode2_expr.insert(code, Arc::clone(cnf_item));
            } else if code_map.contains_key(&code) {
                // We need this check because there may be the case like `select * from t, t1
                // where (t.a=t1.a and t.a=t1.a) or (something).
                // We should make sure that the two `t.a=t1.a` contributes only once.
                // TODO: do this out of this function.
                if !inner_map.contains(&code) {
                    *code_map.get_mut(&code).unwrap() += 1;
                    inner_map.insert(code);
                }
            }
        }
    }
    // We should make sure that this item occurs in every DNF item.
    for (hashcode, cnt) in &code_map {
        if *cnt < dnf_items.len() {
            hashcode2_expr.remove(hashcode);
        }
    }
    if hashcode2_expr.is_empty() {
        return (Vec::new(), Some(Arc::clone(dnf_expr)));
    }
    let mut new_dnf_items = Vec::with_capacity(dnf_items.len());
    let mut only_need_extracted = false;
    for dnf_item in &dnf_items {
        let cnf_items = split_cnf_items(dnf_item);
        let mut new_cnf_items = Vec::with_capacity(cnf_items.len());
        for cnf_item in &cnf_items {
            let code = cnf_item.hash_code();
            if !hashcode2_expr.contains_key(code) {
                new_cnf_items.push(Arc::clone(cnf_item));
            }
        }
        // If the extracted part is just one leaf of the DNF expression. Then the value of the
        // total DNF expression is always the same with the value of the extracted part.
        if new_cnf_items.is_empty() {
            only_need_extracted = true;
            break;
        }
        new_dnf_items.push(
            compose_cnf_condition(ctx, &new_cnf_items).unwrap_or_else(|| {
                panic!("runtime error: invalid memory address or nil pointer dereference")
            }),
        );
    }
    let extracted_expr: Vec<ExprRef> = first_order
        .iter()
        .filter_map(|c| hashcode2_expr.get(c).cloned())
        .collect();
    if only_need_extracted {
        return (extracted_expr, None);
    }
    (extracted_expr, compose_dnf_condition(ctx, &new_dnf_items))
}

/// DeriveRelaxedFiltersFromDNF given a DNF expression, derive a relaxed DNF expression which
/// only contains columns in specified schema; the derived expression is a superset of
/// original expression, i.e, any tuple satisfying the original expression must satisfy the
/// derived expression. Return nil when the derived expression is universal set.
/// A running example is: for schema of t1, `(t1.a=1 and t2.a=1) or (t1.a=2 and t2.a=2)`
/// would be derived as `t1.a=1 or t1.a=2`, while `t1.a=1 or t2.a=1` would get nil.
// go: DeriveRelaxedFiltersFromDNF L1
pub fn derive_relaxed_filters_from_dnf(
    ctx: &dyn BuildContext,
    expr: &ExprRef,
    schema: &Schema,
) -> Option<ExprRef> {
    let Expr::ScalarFunction(sf) = &**expr else {
        return None;
    };
    if sf.func_name.l != ast::LogicOr {
        return None;
    }
    let dnf_items = flatten_dnf_conditions(sf);
    let mut new_dnf_items = Vec::with_capacity(dnf_items.len());
    for dnf_item in &dnf_items {
        let cnf_items = split_cnf_items(dnf_item);
        let mut new_cnf_items = Vec::with_capacity(cnf_items.len());
        for cnf_item in &cnf_items {
            if let Expr::ScalarFunction(item_sf) = &**cnf_item
                && item_sf.func_name.l == ast::LogicOr
            {
                let relaxed_cnf_item = derive_relaxed_filters_from_dnf(ctx, cnf_item, schema);
                if let Some(r) = relaxed_cnf_item {
                    new_cnf_items.push(r);
                }
                // If relaxed expression for embedded DNF is universal set, just drop this
                // CNF item
                continue;
            }
            // This cnfItem must be simple expression now
            // If it cannot be fully covered by schema, just drop this CNF item
            if expr_from_schema(cnf_item, schema) {
                new_cnf_items.push(Arc::clone(cnf_item));
            }
        }
        // If this DNF item involves no column of specified schema, the relaxed expression
        // must be universal set
        if new_cnf_items.is_empty() {
            return None;
        }
        new_dnf_items.push(compose_cnf_condition(ctx, &new_cnf_items)?);
    }
    compose_dnf_condition(ctx, &new_dnf_items)
}

/// GetRowLen gets the length if the func is row, returns 1 if not row.
// go: GetRowLen L1
pub fn get_row_len(e: &Expr) -> usize {
    if let Expr::ScalarFunction(f) = e
        && f.func_name.l == ast::RowFunc
    {
        return f.get_args().len();
    }
    1
}

/// CheckArgsNotMultiColumnRow checks the args are not multi-column row.
// go: CheckArgsNotMultiColumnRow L1
pub fn check_args_not_multi_column_row(args: &[ExprRef]) -> Result<(), tidb_base::Error> {
    for arg in args {
        if get_row_len(arg) != 1 {
            return Err(crate::errors::ErrOperandColumns
                .gen_with_stack_by_args(vec![tidb_base::gofmt::Arg::Int(1, "int")]));
        }
    }
    Ok(())
}

/// GetFuncArg gets the argument of the function at idx.
// go: GetFuncArg L1
pub fn get_func_arg(e: &Expr, idx: usize) -> Option<ExprRef> {
    if let Expr::ScalarFunction(f) = e {
        return Some(Arc::clone(&f.get_args()[idx]));
    }
    None
}

/// DatumToConstant generates a Constant expression from a Datum.
// go: DatumToConstant L1
pub fn datum_to_constant(d: Datum, tp: u8, flag: usize) -> Constant {
    let mut t = new_field_type(tp);
    t.add_flag(flag);
    Constant::new(d, t)
}

/// GetStringFromConstant gets a string value from the Constant expression.
/// `Ok((value, is_null))`.
// go: GetStringFromConstant L1
pub fn get_string_from_constant(
    ctx: &dyn EvalContext,
    value: &Expr,
) -> Result<(Vec<u8>, bool), tidb_base::Error> {
    let Expr::Constant(con) = value else {
        // Go: errors.Errorf("Not a Constant expression %+v", value)
        return Err(tidb_base::Error::plain(format!(
            "Not a Constant expression {}",
            String::from_utf8_lossy(
                &value.string_with_ctx(ctx, tidb_base::errors::RedactLogDisable)
            )
        )));
    };
    match con.eval_string(ctx, &EvalState::new(), tidb_chunk::Row::default())? {
        None => Ok((Vec::new(), true)),
        Some(s) => Ok((s, false)),
    }
}

/// GetIntFromConstant gets an integer value from the Constant expression.
/// `Ok((value, is_null))`.
// go: GetIntFromConstant L1
pub fn get_int_from_constant(
    ctx: &dyn EvalContext,
    value: &Expr,
) -> Result<(i64, bool), tidb_base::Error> {
    let (s, is_null) = get_string_from_constant(ctx, value)?;
    if is_null {
        return Ok((0, true));
    }
    let (int_num, err) = atoi(&s);
    if err.is_err() {
        return Ok((0, true));
    }
    Ok((int_num, false))
}

/// IsRuntimeConstExpr checks if a expr can be treated as a constant in **executor**.
// go: IsRuntimeConstExpr L1
pub fn is_runtime_const_expr(expr: &Expr) -> bool {
    match expr {
        Expr::ScalarFunction(x) => {
            if unFoldableFunctions.contains(&x.func_name.l.as_str()) {
                return false;
            }
            x.get_args().iter().all(|arg| is_runtime_const_expr(arg))
        }
        Expr::Column(_) => false,
        Expr::Constant(_) | Expr::CorrelatedColumn(_) => true,
    }
}

/// CheckNonDeterministic checks whether the current expression contains a non-deterministic
/// func.
// go: CheckNonDeterministic L1
pub fn check_non_deterministic(e: &Expr) -> bool {
    match e {
        Expr::Constant(_) | Expr::Column(_) | Expr::CorrelatedColumn(_) => false,
        Expr::ScalarFunction(x) => {
            if unFoldableFunctions.contains(&x.func_name.l.as_str()) {
                return true;
            }
            x.get_args().iter().any(|arg| check_non_deterministic(arg))
        }
    }
}

/// CheckFuncInExpr checks whether there's a given function in the expression.
// go: CheckFuncInExpr L1
pub fn check_func_in_expr(e: &Expr, func_name: &str) -> bool {
    match e {
        Expr::Constant(_) | Expr::Column(_) | Expr::CorrelatedColumn(_) => false,
        Expr::ScalarFunction(x) => {
            if x.func_name.l == func_name {
                return true;
            }
            x.get_args()
                .iter()
                .any(|arg| check_func_in_expr(arg, func_name))
        }
    }
}

/// IsImmutableFunc checks whether this expression only consists of foldable functions.
/// This expression can be evaluated by using `expr.Eval(chunk.Row{})` directly and the result
/// won't change if it's immutable.
// go: IsImmutableFunc L1
pub fn is_immutable_func(expr: &Expr) -> bool {
    match expr {
        Expr::ScalarFunction(x) => {
            let l = x.func_name.l.as_str();
            if unFoldableFunctions.contains(&l) || mutableEffectsFunctions.contains(&l) {
                return false;
            }
            x.get_args().iter().all(|arg| is_immutable_func(arg))
        }
        _ => true,
    }
}

/// GetUint64FromConstant gets a uint64 from constant expression. `(value, is_null, ok)`.
/// Go logs a warning on the failure paths; the logs are not reproduced (D08.12 spec §4).
// go: GetUint64FromConstant L2 (no warning logs)
#[allow(non_upper_case_globals)]
pub fn get_uint64_from_constant(ctx: &dyn EvalContext, expr: &Expr) -> (u64, bool, bool) {
    let Expr::Constant(con) = expr else {
        return (0, false, false);
    };
    let mut dt = con.value.clone();
    if let Some(pm) = &con.param_marker {
        match pm.get_user_var(ctx) {
            Ok(d) => dt = d,
            Err(_) => return (0, false, false),
        }
    } else if let Some(de) = &con.deferred_expr {
        match de.eval(ctx, &EvalState::new(), tidb_chunk::Row::default()) {
            Ok(d) => dt = d,
            Err(_) => return (0, false, false),
        }
    }
    match dt.kind() {
        KindNull => (0, true, true),
        KindInt64 => {
            let val = dt.get_int64();
            if val < 0 {
                return (0, false, false);
            }
            (val as u64, false, true)
        }
        KindUint64 => (dt.get_uint64(), false, true),
        _ => (0, false, false),
    }
}

/// ContainVirtualColumn checks if the expressions contain a virtual column
// go: ContainVirtualColumn L1
pub fn contain_virtual_column(exprs: &[ExprRef]) -> bool {
    for expr in exprs {
        match &**expr {
            Expr::Column(v) => {
                if v.virtual_expr.is_some() {
                    return true;
                }
            }
            Expr::ScalarFunction(v) if contain_virtual_column(v.get_args()) => return true,
            _ => {}
        }
    }
    false
}

/// ContainCorrelatedColumn checks if the expressions contain a correlated column
// go: ContainCorrelatedColumn L1
pub fn contain_correlated_column(exprs: &[ExprRef]) -> bool {
    for expr in exprs {
        match &**expr {
            Expr::CorrelatedColumn(_) => return true,
            Expr::ScalarFunction(v) if contain_correlated_column(v.get_args()) => return true,
            _ => {}
        }
    }
    false
}

// go: jsonUnquoteFunctionBenefitsFromPushedDown L1
fn json_unquote_function_benefits_from_pushed_down(sf: &ScalarFunction) -> bool {
    let arg0 = &sf.get_args()[0];
    // Only `->>` which parsed to JSONUnquote(CAST(JSONExtract() AS string)) can be pushed
    // down to tikv
    if let Expr::ScalarFunction(f_child) = &**arg0
        && f_child.func_name.l == ast::Cast
        && let Expr::ScalarFunction(f_grand) = &*f_child.get_args()[0]
        && f_grand.func_name.l == ast::JSONExtract
    {
        return true;
    }
    false
}

/// ProjectionBenefitsFromPushedDown evaluates if the expressions can improve performance when
/// pushed down to TiKV. Projections are not pushed down to tikv by default, thus we need to
/// check strictly here to avoid potential performance degradation.
/// Note: virtual column is not considered here, since this function cares performance instead
/// of functionality
// go: ProjectionBenefitsFromPushedDown L1
pub fn projection_benefits_from_pushed_down(exprs: &[ExprRef], input_schema_len: usize) -> bool {
    let mut all_col_ref = true;
    let mut col_ref_count = 0usize;
    for expr in exprs {
        match &**expr {
            Expr::Column(_) => {
                col_ref_count += 1;
                continue;
            }
            Expr::ScalarFunction(v) => {
                all_col_ref = false;
                match v.func_name.l.as_str() {
                    ast::JSONDepth
                    | ast::JSONLength
                    | ast::JSONType
                    | ast::JSONValid
                    | ast::JSONContains
                    | ast::JSONContainsPath
                    | ast::JSONExtract
                    | ast::JSONKeys
                    | ast::JSONSearch
                    | ast::JSONMemberOf
                    | ast::JSONOverlaps => {
                        continue;
                    }
                    ast::JSONUnquote => {
                        if json_unquote_function_benefits_from_pushed_down(v) {
                            continue;
                        }
                        return false;
                    }
                    _ => return false,
                }
            }
            _ => return false,
        }
    }
    // For all col refs, only push down column pruning projections
    if all_col_ref {
        return col_ref_count < input_schema_len;
    }
    true
}

/// MaybeOverOptimized4PlanCacheForMultiExpression is the same as
/// MaybeOverOptimized4PlanCache, but takes multiple Expressions as input.
// go: MaybeOverOptimized4PlanCacheForMultiExpression L1
pub fn maybe_over_optimized4_plan_cache_for_multi_expression(
    ctx: &dyn BuildContext,
    exprs: &[ExprRef],
) -> bool {
    crate::constant_fold::maybe_over_optimized4_plan_cache(ctx, exprs)
}

const KIB: f64 = 1024.0;
const MIB: f64 = 1024.0 * KIB;
const GIB: f64 = 1024.0 * MIB;
const TIB: f64 = 1024.0 * GIB;
const PIB: f64 = 1024.0 * TIB;
const EIB: f64 = 1024.0 * PIB;

const NANO: f64 = 1.0;
const MICRO: f64 = 1000.0 * NANO;
const MILLI: f64 = 1000.0 * MICRO;
const SEC: f64 = 1000.0 * MILLI;
const MINUTE: f64 = 60.0 * SEC;
const HOUR: f64 = 60.0 * MINUTE;
const DAY_TIME: f64 = 24.0 * HOUR;

// glue: the shared tail of GetFormatBytes / GetFormatNanoTime
fn format_with_unit(v: f64, divisor: f64, unit: &str) -> String {
    if divisor == 1.0 {
        return format_float(v, b'f', 0, 64) + " " + unit;
    }
    let value = v / divisor;
    if value.abs() >= 100000.0 {
        return format_float(value, b'e', 2, 64) + " " + unit;
    }
    format_float(value, b'f', 2, 64) + " " + unit
}

/// GetFormatBytes convert byte count to value with units.
// go: GetFormatBytes L1
pub fn get_format_bytes(bytes: f64) -> String {
    let bytes_abs = bytes.abs();
    let (divisor, unit) = if bytes_abs >= EIB {
        (EIB, "EiB")
    } else if bytes_abs >= PIB {
        (PIB, "PiB")
    } else if bytes_abs >= TIB {
        (TIB, "TiB")
    } else if bytes_abs >= GIB {
        (GIB, "GiB")
    } else if bytes_abs >= MIB {
        (MIB, "MiB")
    } else if bytes_abs >= KIB {
        (KIB, "KiB")
    } else {
        (1.0, "bytes")
    };
    format_with_unit(bytes, divisor, unit)
}

/// GetFormatNanoTime convert time in nanoseconds to value with units.
// go: GetFormatNanoTime L1
pub fn get_format_nano_time(time: f64) -> String {
    let time_abs = time.abs();
    let (divisor, unit) = if time_abs >= DAY_TIME {
        (DAY_TIME, "d")
    } else if time_abs >= HOUR {
        (HOUR, "h")
    } else if time_abs >= MINUTE {
        (MINUTE, "min")
    } else if time_abs >= SEC {
        (SEC, "s")
    } else if time_abs >= MILLI {
        (MILLI, "ms")
    } else if time_abs >= MICRO {
        (MICRO, "us")
    } else {
        (1.0, "ns")
    };
    format_with_unit(time, divisor, unit)
}

/// ExprsToStringsForDisplay convert a slice of Expression to a slice of string using
/// Expression.String(), and to make it better for display and debug, it also escapes the
/// string to corresponding golang string literal, which means using \t, \n, \x??, \u????,
/// ... to represent newline, control character, non-printable character, invalid utf-8 bytes
/// and so on.
// go: ExprsToStringsForDisplay L1
pub fn exprs_to_strings_for_display(ctx: &dyn EvalContext, exprs: &[ExprRef]) -> Vec<String> {
    exprs
        .iter()
        .map(|cond| {
            // We only need the escape functionality of strconv.Quote, the quoting is not
            // needed, so we trim the \" prefix and suffix here.
            let q = strconv_quote(&cond.string_with_ctx(ctx, tidb_base::errors::RedactLogDisable));
            let q = q.strip_prefix('"').unwrap_or(&q);
            q.strip_suffix('"').unwrap_or(q).to_string()
        })
        .collect()
}

/// ExprsHasSideEffects checks if any of the expressions has side effects.
// go: ExprsHasSideEffects L1
pub fn exprs_has_side_effects(exprs: &[ExprRef]) -> bool {
    exprs.iter().any(|expr| expr_has_set_var_or_sleep(expr))
}

/// ExprHasSetVarOrSleep checks if the expression has SetVar function or Sleep function.
// go: ExprHasSetVarOrSleep L1
pub fn expr_has_set_var_or_sleep(expr: &Expr) -> bool {
    let Expr::ScalarFunction(scala_func) = expr else {
        return false;
    };
    if scala_func.func_name.l == ast::SetVar || scala_func.func_name.l == ast::Sleep {
        return true;
    }
    scala_func
        .get_args()
        .iter()
        .any(|arg| expr_has_set_var_or_sleep(arg))
}
