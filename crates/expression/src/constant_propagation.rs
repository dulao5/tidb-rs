//! go-file: pkg/expression/constant_propagation.go
//!
//! Constant propagation (block D08.2): `PropagateConstant` over a CNF condition list and
//! `PropConstOverOuterJoin` over the join and filter conditions of an outer join.

#![allow(non_upper_case_globals)]
// The loops keep Go's index-based shape: conditions are appended while they run.
#![allow(clippy::needless_range_loop)]

use std::collections::HashMap;
use std::sync::Arc;

use tidb_base::disjointset::{SimpleIntSet, new_int_set};
use tidb_base::mysql;
use tidb_chunk::Row;
use tidb_sessionctx::exprctx::{ExprContextRef, with_constant_propagate_check};
use tidb_types::collate::get_collator;
use tidb_types::datum::{
    KindInt64, KindMysqlEnum, KindMysqlSet, KindString, new_int_datum, new_mysql_enum_datum,
};
use tidb_types::enum_type::{parse_enum_name, parse_enum_value};
use tidb_types::field_type::new_field_type;

use crate::ast_funcs as ast;
use crate::column::Column;
use crate::constant::Constant;
use crate::constant_fold::maybe_over_optimized4_plan_cache;
use crate::context::{BuildContext, EvalContext, type_ctx};
use crate::expression::{
    EvalState, Expr, ExprRef, compose_cnf_condition, compose_dnf_condition, eval_bool,
    split_cnf_items, split_dnf_items,
};
use crate::function_traits::{inequalFunctions, unFoldableFunctions};
use crate::scalar_function::{ScalarFunction, new_function_internal};
use crate::schema::{Schema, expr_from_schema, merge_schema, new_schema};
use crate::util::{
    build_not_null_expr, column_substitute, const_expr_consider_plan_cache, extract_columns,
    remove_dup_exprs,
};

/// MaxPropagateColsCnt means the max number of columns that can participate propagation.
pub const MaxPropagateColsCnt: usize = 100;

/// The `basePropConstSolver` fields. `eq_list[i]` is a `*Constant` node (`Expr::Constant`).
struct BasePropConstSolver {
    /// colMapper maps column to its index
    col_mapper: HashMap<i64, usize>,
    /// if eqList[i] != nil, it means col_i = eqList[i]
    eq_list: Vec<Option<ExprRef>>,
    /// unionSet stores the relations like col_i = col_j
    union_set: SimpleIntSet,
    /// columns stores all columns appearing in the conditions
    columns: Vec<Arc<Column>>,
    /// Set by `PropagateConstant` (`propConstSolver`) or at construction (outer join).
    ctx: Option<ExprContextRef>,
}

impl BasePropConstSolver {
    // glue: the struct literal of the solver constructors
    fn new(ctx: Option<ExprContextRef>) -> Self {
        BasePropConstSolver {
            col_mapper: HashMap::new(),
            eq_list: Vec::new(),
            union_set: SimpleIntSet::default(),
            columns: Vec::new(),
            ctx,
        }
    }

    // glue: the solver's ctx field
    fn ctx(&self) -> &ExprContextRef {
        self.ctx
            .as_ref()
            .expect("the solver's context is set before solve")
    }

    // glue: the solver's ctx as a BuildContext
    fn bctx(&self) -> &dyn BuildContext {
        &**self.ctx()
    }

    // go: basePropConstSolver.getColID L1
    fn get_col_id(&self, col: &Column) -> usize {
        // Go reads a missing key as 0; every column of the conditions is inserted first.
        self.col_mapper.get(&col.unique_id).copied().unwrap_or(0)
    }

    // go: basePropConstSolver.insertCol L1
    fn insert_col(&mut self, col: &Arc<Column>) {
        if !self.col_mapper.contains_key(&col.unique_id) {
            let id = self.col_mapper.len();
            self.col_mapper.insert(col.unique_id, id);
            self.columns.push(Arc::clone(col));
        }
    }

    /// tryToUpdateEQList tries to update the eqList. When the eqList has store this column
    /// with a different constant, like a = 1 and a = 2, we set the second return value to
    /// false.
    // go: basePropConstSolver.tryToUpdateEQList L1
    fn try_to_update_eq_list(&mut self, col: &Column, con: &ExprRef) -> (bool, bool) {
        let c = con.as_constant().expect("a constant");
        if c.value.is_null() && const_expr_consider_plan_cache(con, self.ctx().is_use_cache()) {
            return (false, true);
        }
        let id = self.get_col_id(col);
        if let Some(old_con) = &self.eq_list[id] {
            let ev = self.ctx().get_eval_ctx();
            let old = old_con.as_constant().expect("a constant");
            let collator = get_collator(col.get_type().get_collate());
            let (res, err) = old.value.compare(&type_ctx(&*ev), &c.value, collator);
            return (false, res != 0 || err.is_err());
        }
        self.eq_list[id] = Some(Arc::clone(con));
        (true, false)
    }

    // go: basePropConstSolver.dealWithPossibleHybridType L1
    fn deal_with_possible_hybrid_type(
        &self,
        col: &Column,
        con: ExprRef,
    ) -> (Option<ExprRef>, bool) {
        let ev = self.ctx().get_eval_ctx();
        let col_tp = col.get_type().clone();
        if !col_tp.hybrid() {
            return (Some(con), true);
        }
        if col_tp.get_type() == mysql::TypeEnum {
            let d = match con.eval(&*ev, &EvalState::new(), Row::default()) {
                Ok(d) => d,
                Err(_) => return (None, false),
            };
            if maybe_over_optimized4_plan_cache(self.bctx(), std::slice::from_ref(&con)) {
                self.ctx().set_skip_plan_cache(
                    "Skip plan cache since mutable constant is restored and propagated",
                );
            }
            let elems = col_tp.get_elems().unwrap_or(&[]);
            let enum_con = |e| {
                let mut c = Constant::new(new_mysql_enum_datum(e), (*col.ret_type).clone());
                c.collation_info = col.collation_info.clone();
                Expr::Constant(c).into_ref()
            };
            let con = match d.kind() {
                KindInt64 => match parse_enum_value(elems, d.get_int64() as u64) {
                    // Go logs "Invalid Enum parsed during constant propagation" at debug level.
                    Ok(e) => enum_con(e),
                    Err(_) => return (None, false),
                },
                KindString => match parse_enum_name(elems, d.get_string(), d.collation()) {
                    Ok(e) => enum_con(e),
                    Err(_) => return (None, false),
                },
                // It's already a hybrid type. Just use it.
                KindMysqlEnum | KindMysqlSet => con,
                // We skip other cases first.
                _ => return (None, false),
            };
            return (Some(con), true);
        }
        (None, false)
    }
}

/// ValidCompareConstantPredicate checks if the predicate is an expression like
/// [column '>'|'>='|'<'|'<='|'=' constant].
// go: ValidCompareConstantPredicate L1
pub fn valid_compare_constant_predicate(ctx: &dyn EvalContext, candidate_predicate: &Expr) -> bool {
    let Some(scalar_function) = candidate_predicate.as_scalar_function() else {
        return false;
    };
    let l = scalar_function.func_name.l.as_str();
    if l != ast::GT && l != ast::GE && l != ast::LT && l != ast::LE && l != ast::EQ {
        return false;
    }
    valid_compare_constant_predicate_helper(ctx, scalar_function, true).is_some()
        || valid_compare_constant_predicate_helper(ctx, scalar_function, false).is_some()
}

/// ValidCompareConstantPredicateHelper checks if the predicate is a compare constant
/// predicate, like "Column xxx Constant". Returns the column and the constant (the
/// `*Constant` node as an expression).
// go: ValidCompareConstantPredicateHelper L1
pub fn valid_compare_constant_predicate_helper<'a>(
    ctx: &dyn EvalContext,
    eq: &'a ScalarFunction,
    col_is_left: bool,
) -> Option<(&'a Column, &'a ExprRef)> {
    let args = eq.get_args();
    let (col_arg, con_arg) = if col_is_left {
        (&args[0], &args[1])
    } else {
        (&args[1], &args[0])
    };
    let col = col_arg.as_column()?;
    let con = con_arg.as_constant()?;
    // Go: a nil `GetType` (an unreadable parameter) dereferences nil; the empty collation
    // never equals a column's collation.
    let con_coll = con
        .get_type(ctx)
        .map(|t| t.get_collate().to_string())
        .unwrap_or_default();
    if col.get_static_type().get_collate() != con_coll {
        return None;
    }
    Some((col, con_arg))
}

/// validEqualCond checks if the cond is an expression like [column eq constant].
// go: validEqualCond L1
fn valid_equal_cond<'a>(
    ctx: &dyn EvalContext,
    cond: &'a Expr,
) -> Option<(&'a Column, &'a ExprRef)> {
    let eq = cond.as_scalar_function()?;
    if eq.func_name.l != ast::EQ {
        return None;
    }
    valid_compare_constant_predicate_helper(ctx, eq, true)
        .or_else(|| valid_compare_constant_predicate_helper(ctx, eq, false))
}

/// tryToReplaceCond aims to replace all occurrences of column 'src' and try to replace it
/// with 'tgt' in 'cond'. It returns
///
///   bool: if a replacement happened
///   bool: if 'cond' contains non-deterministic expression
///   Expression: the replaced expression, or original 'cond' if the replacement didn't happen
// go: tryToReplaceCond L2 (a failed NewFunctionInternal keeps 'cond'; Go returns nil, spec §4)
fn try_to_replace_cond(
    ctx: &dyn BuildContext,
    src: &Column,
    tgt: &Arc<Column>,
    cond: &ExprRef,
    null_aware: bool,
) -> (bool, bool, ExprRef) {
    if src.ret_type.get_type() != tgt.ret_type.get_type() {
        return (false, false, Arc::clone(cond));
    }
    let Some(sf) = cond.as_scalar_function() else {
        return (false, false, Arc::clone(cond));
    };
    let mut replaced = false;
    let mut args: Option<Vec<ExprRef>> = None;
    if unFoldableFunctions.contains(&sf.func_name.l.as_str()) {
        return (false, true, Arc::clone(cond));
    }
    if inequalFunctions.contains(&sf.func_name.l.as_str()) {
        return (false, true, Arc::clone(cond));
    }
    // See
    //	https://github.com/pingcap/tidb/issues/15782
    //  https://github.com/pingcap/tidb/issues/17817
    // The null sensitive function's result may rely on the original nullable information of the outer side column.
    // Its args cannot be replaced easily.
    // A more strict check is that after we replace the arg. We check the nullability of the new expression.
    // But we haven't maintained it yet, so don't replace the arg of the control function currently.
    if null_aware
        && (sf.func_name.l == ast::Ifnull
            || sf.func_name.l == ast::If
            || sf.func_name.l == ast::Case
            || sf.func_name.l == ast::NullEQ)
    {
        return (false, true, Arc::clone(cond));
    }
    for (idx, expr) in sf.get_args().iter().enumerate() {
        if src.equal_column(expr) {
            let (_, coll) = cond.charset_and_collation();
            if tgt.get_type().get_collate() != coll {
                continue;
            }
            replaced = true;
            args.get_or_insert_with(|| sf.get_args().to_vec())[idx] =
                Expr::Column((**tgt).clone()).into_ref();
        } else {
            let (sub_replaced, is_non_deterministic, sub_expr) =
                try_to_replace_cond(ctx, src, tgt, expr, null_aware);
            if is_non_deterministic {
                return (false, true, Arc::clone(cond));
            } else if sub_replaced {
                replaced = true;
                args.get_or_insert_with(|| sf.get_args().to_vec())[idx] = sub_expr;
            }
        }
    }
    if replaced
        && let Some(e) = new_function_internal(
            ctx,
            &sf.func_name.l,
            sf.ret_type().clone(),
            args.expect("set with replaced"),
        )
    {
        return (true, false, e);
    }
    (false, false, Arc::clone(cond))
}

/// The constant `false` (`&Constant{Value: types.NewDatum(false), RetType: TypeTiny}`).
// glue: the literal false Constant of setConds2ConstFalse
fn false_constant() -> ExprRef {
    Expr::Constant(Constant::new(
        new_int_datum(0),
        new_field_type(mysql::TypeTiny),
    ))
    .into_ref()
}

/// Evaluates a constant CNF item to a boolean (`EvalBool(ctx, []Expression{con}, chunk.Row{})`).
// glue: the EvalBool call of pickNewEQConds
fn eval_const_bool(ctx: &dyn EvalContext, con: &ExprRef) -> Result<bool, tidb_base::Error> {
    eval_bool(
        ctx,
        &EvalState::new(),
        std::slice::from_ref(con),
        Row::default(),
    )
    .map(|r| r.0)
}

/// Substitutes the picked `column = constant` pairs (`ColumnSubstitute(ctx, cond,
/// NewSchema(cols...), cons)`). Go collects the pairs from a map, in random order; the
/// result does not depend on it (each column has one constant).
// glue: the substitution loop body of propagateConstantEQ
fn substitute(
    ctx: &dyn BuildContext,
    columns: &[Arc<Column>],
    mapper: &[(usize, ExprRef)],
    cond: &ExprRef,
) -> ExprRef {
    let cols = mapper
        .iter()
        .map(|(id, _)| Arc::clone(&columns[*id]))
        .collect();
    let cons: Vec<ExprRef> = mapper.iter().map(|(_, c)| Arc::clone(c)).collect();
    column_substitute(ctx, cond, &new_schema(cols), &cons)
}

/// The `propConstSolver`.
struct PropConstSolver {
    base: BasePropConstSolver,
    conditions: Vec<ExprRef>,
}

impl PropConstSolver {
    /// propagateConstantEQ propagates expressions like 'column = constant' by substituting
    /// the constant for column, the procedure repeats multiple times.
    // go: propConstSolver.propagateConstantEQ L1
    fn propagate_constant_eq(&mut self) {
        self.base.eq_list = vec![None; self.base.columns.len()];
        let mut visited = vec![false; self.conditions.len()];
        for _ in 0..MaxPropagateColsCnt {
            let Some(mapper) = self.pick_new_eq_conds(&mut visited) else {
                return;
            };
            if mapper.is_empty() {
                return;
            }
            for i in 0..self.conditions.len() {
                if !visited[i] {
                    self.conditions[i] = substitute(
                        self.base.bctx(),
                        &self.base.columns,
                        &mapper,
                        &self.conditions[i],
                    );
                }
            }
        }
    }

    /// propagateColumnEQ propagates expressions like 'column A = column B' by adding extra
    /// filters 'expression(..., column B, ...)' propagated from 'expression(..., column A,
    /// ...)' as long as the expression is deterministic and has no side effect.
    // go: propConstSolver.propagateColumnEQ L1
    fn propagate_column_eq(&mut self) {
        let mut visited = vec![false; self.conditions.len()];
        self.base.union_set = new_int_set(self.base.columns.len());
        for i in 0..self.conditions.len() {
            if let Expr::ScalarFunction(fun) = &*self.conditions[i]
                && fun.func_name.l == ast::EQ
            {
                let l_col = fun.get_args()[0].as_column();
                let r_col = fun.get_args()[1].as_column();
                // TODO: Enable hybrid types in ConstantPropagate.
                if let (Some(l_col), Some(r_col)) = (l_col, r_col) {
                    let (lt, rt) = (l_col.get_type(), r_col.get_type());
                    if lt.get_collate() == rt.get_collate() && !lt.hybrid() && !rt.hybrid() {
                        let l_id = self.base.get_col_id(l_col);
                        let r_id = self.base.get_col_id(r_col);
                        self.base.union_set.union(l_id, r_id);
                        visited[i] = true;
                    }
                }
            }
        }

        let conds_len = self.conditions.len();
        let n = self.base.columns.len();
        for i in 0..n {
            for j in i + 1..n {
                // unionSet doesn't have iterate(), we use a two layer loop to iterate col_i = col_j relation
                if self.base.union_set.find_root(i) != self.base.union_set.find_root(j) {
                    continue;
                }
                let coli = Arc::clone(&self.base.columns[i]);
                let colj = Arc::clone(&self.base.columns[j]);
                for k in 0..conds_len {
                    if visited[k] {
                        // cond_k has been used to retrieve equality relation
                        continue;
                    }
                    let cond = Arc::clone(&self.conditions[k]);
                    let (replaced, _, new_expr) =
                        try_to_replace_cond(self.base.bctx(), &coli, &colj, &cond, false);
                    if replaced {
                        self.conditions.push(new_expr);
                    }
                    let (replaced, _, new_expr) =
                        try_to_replace_cond(self.base.bctx(), &colj, &coli, &cond, false);
                    if replaced {
                        self.conditions.push(new_expr);
                    }
                }
            }
        }
    }

    // go: propConstSolver.setConds2ConstFalse L1
    fn set_conds2_const_false(&mut self) {
        if maybe_over_optimized4_plan_cache(self.base.bctx(), &self.conditions) {
            self.base.ctx().set_skip_plan_cache(
                "some parameters may be overwritten when constant propagation",
            );
        }
        self.conditions = vec![false_constant()];
    }

    /// pickNewEQConds tries to pick new equal conds and puts them to retMapper. `None` is
    /// Go's nil map (the conditions became false or an error occurred).
    // go: propConstSolver.pickNewEQConds L1
    fn pick_new_eq_conds(&mut self, visited: &mut [bool]) -> Option<Vec<(usize, ExprRef)>> {
        let mut ret_mapper = Vec::new();
        let ev = self.base.ctx().get_eval_ctx();
        for i in 0..self.conditions.len() {
            if visited[i] {
                continue;
            }
            let cond = Arc::clone(&self.conditions[i]);
            let Some((col, con)) = valid_equal_cond(&*ev, &cond) else {
                // Then we check if this CNF item is a false constant. If so, we will set the whole condition to false.
                if !matches!(&*cond, Expr::Constant(_)) {
                    continue;
                }
                visited[i] = true;
                match eval_const_bool(&*ev, &cond) {
                    Err(err) => {
                        tidb_base::terror::log(Some(&err));
                        return None;
                    }
                    Ok(false) => {
                        self.set_conds2_const_false();
                        return None;
                    }
                    Ok(true) => continue,
                }
            };
            // TODO: Enable hybrid types in ConstantPropagate.
            if col.get_type().hybrid() {
                continue;
            }
            visited[i] = true;
            let (updated, forever_false) = self.base.try_to_update_eq_list(col, con);
            if forever_false {
                self.set_conds2_const_false();
                return None;
            }
            if updated {
                ret_mapper.push((self.base.get_col_id(col), Arc::clone(con)));
            }
        }
        Some(ret_mapper)
    }

    // go: propConstSolver.solve L1
    fn solve(&mut self, conditions: &[ExprRef]) -> Vec<ExprRef> {
        let mut cols = Vec::with_capacity(conditions.len());
        for cond in conditions {
            self.conditions.extend(split_cnf_items(cond));
            cols.extend(extract_columns(cond));
        }
        for col in &cols {
            self.base.insert_col(col);
        }
        if self.base.columns.len() > MaxPropagateColsCnt {
            // Go logs "too many columns in a single CNF" at warn level.
            return conditions.to_vec();
        }
        self.propagate_constant_eq();
        self.propagate_column_eq();
        let ctx = Arc::clone(self.base.ctx());
        let conds = std::mem::take(&mut self.conditions);
        self.conditions = propagate_constant_dnf(&ctx, conds);
        remove_dup_exprs(std::mem::take(&mut self.conditions))
    }
}

/// PropagateConstant propagate constant values of deterministic predicates in a condition.
/// This is a constant propagation logic for expression list such as ['a=1', 'a=b']
// go: PropagateConstant L1
pub fn propagate_constant(ctx: &ExprContextRef, conditions: &[ExprRef]) -> Vec<ExprRef> {
    new_prop_const_solver().propagate_constant(
        Arc::new(with_constant_propagate_check(Arc::clone(ctx))),
        conditions,
    )
}

/// newPropConstSolver returns a PropagateConstantSolver. The context is set by
/// `PropagateConstant`, as in Go; until then it is a placeholder.
// go: newPropConstSolver L1
fn new_prop_const_solver() -> PropConstSolver {
    PropConstSolver {
        base: BasePropConstSolver::new(None),
        conditions: Vec::new(),
    }
}

impl PropConstSolver {
    /// PropagateConstant propagate constant values of deterministic predicates in a condition.
    // go: propConstSolver.PropagateConstant L1
    fn propagate_constant(mut self, ctx: ExprContextRef, conditions: &[ExprRef]) -> Vec<ExprRef> {
        self.base.ctx = Some(ctx);
        self.solve(conditions)
    }
}

/// The `propOuterJoinConstSolver`.
struct PropOuterJoinConstSolver<'a> {
    base: BasePropConstSolver,
    join_conds: Vec<ExprRef>,
    filter_conds: Vec<ExprRef>,
    outer_schema: &'a Schema,
    inner_schema: &'a Schema,
    /// nullSensitive indicates if this outer join is null sensitive, if true, we cannot
    /// generate additional `col is not null` condition from column equal conditions.
    /// Specifically, this value is true for LeftOuterSemiJoin and AntiLeftOuterSemiJoin.
    null_sensitive: bool,
}

impl PropOuterJoinConstSolver<'_> {
    // go: propOuterJoinConstSolver.setConds2ConstFalse L1
    fn set_conds2_const_false(&mut self, filter_conds: bool) {
        self.join_conds = vec![false_constant()];
        if filter_conds {
            self.filter_conds = vec![false_constant()];
        }
    }

    /// pickEQCondsOnOuterCol picks constant equal expression from specified conditions.
    /// `None` is Go's nil map.
    // go: propOuterJoinConstSolver.pickEQCondsOnOuterCol L1
    fn pick_eq_conds_on_outer_col(
        &mut self,
        mut ret_mapper: Vec<(usize, ExprRef)>,
        visited: &mut [bool],
        filter_conds: bool,
    ) -> Option<Vec<(usize, ExprRef)>> {
        let (conds, conds_offset) = if filter_conds {
            (self.filter_conds.clone(), 0)
        } else {
            (self.join_conds.clone(), self.filter_conds.len())
        };
        let ev = self.base.ctx().get_eval_ctx();
        for (i, cond) in conds.iter().enumerate() {
            if visited[i + conds_offset] {
                continue;
            }
            let Some((col, con)) = valid_equal_cond(&*ev, cond) else {
                // Then we check if this CNF item is a false constant. If so, we will set the whole condition to false.
                if !matches!(&**cond, Expr::Constant(_)) {
                    continue;
                }
                visited[i + conds_offset] = true;
                match eval_const_bool(&*ev, cond) {
                    Err(err) => {
                        tidb_base::terror::log(Some(&err));
                        return None;
                    }
                    Ok(false) => {
                        self.set_conds2_const_false(filter_conds);
                        return None;
                    }
                    Ok(true) => continue,
                }
            };
            let (con, valid) = self
                .base
                .deal_with_possible_hybrid_type(col, Arc::clone(con));
            if !valid {
                continue;
            }
            let con = con.expect("valid");
            // Only extract `outerCol = const` expressions.
            if !self.outer_schema.contains(col) {
                continue;
            }
            visited[i + conds_offset] = true;
            let (updated, forever_false) = self.base.try_to_update_eq_list(col, &con);
            if forever_false {
                self.set_conds2_const_false(filter_conds);
                return None;
            }
            if updated {
                ret_mapper.push((self.base.get_col_id(col), con));
            }
        }
        Some(ret_mapper)
    }

    /// pickNewEQConds picks constant equal expressions from join and filter conditions.
    // go: propOuterJoinConstSolver.pickNewEQConds L1
    fn pick_new_eq_conds(&mut self, visited: &mut [bool]) -> Option<Vec<(usize, ExprRef)>> {
        // Filter is constant false or error occurred, enforce early termination.
        let ret_mapper = self.pick_eq_conds_on_outer_col(Vec::new(), visited, true)?;
        self.pick_eq_conds_on_outer_col(ret_mapper, visited, false)
    }

    /// propagateConstantEQ propagates expressions like `outerCol = const` by substituting
    /// `outerCol` in *JOIN* condition with `const`, the procedure repeats multiple times.
    // go: propOuterJoinConstSolver.propagateConstantEQ L1
    fn propagate_constant_eq(&mut self) {
        self.base.eq_list = vec![None; self.base.columns.len()];
        let len_filters = self.filter_conds.len();
        let mut visited = vec![false; len_filters + self.join_conds.len()];
        for _ in 0..MaxPropagateColsCnt {
            let Some(mapper) = self.pick_new_eq_conds(&mut visited) else {
                return;
            };
            if mapper.is_empty() {
                return;
            }
            for i in 0..self.join_conds.len() {
                if !visited[i + len_filters] {
                    self.join_conds[i] = substitute(
                        self.base.bctx(),
                        &self.base.columns,
                        &mapper,
                        &self.join_conds[i],
                    );
                }
            }
        }
    }

    // go: propOuterJoinConstSolver.colsFromOuterAndInner L1
    fn cols_from_outer_and_inner(
        &self,
        col1: &Arc<Column>,
        col2: &Arc<Column>,
    ) -> Option<(Arc<Column>, Arc<Column>)> {
        if self.outer_schema.contains(col1) && self.inner_schema.contains(col2) {
            return Some((Arc::clone(col1), Arc::clone(col2)));
        }
        if self.outer_schema.contains(col2) && self.inner_schema.contains(col1) {
            return Some((Arc::clone(col2), Arc::clone(col1)));
        }
        None
    }

    /// validColEqualCond checks if expression is column equal condition that we can use for
    /// constant propagation over outer join. We only use expression like `outerCol =
    /// innerCol`.
    // go: propOuterJoinConstSolver.validColEqualCond L1
    fn valid_col_equal_cond(&self, cond: &Expr) -> Option<(Arc<Column>, Arc<Column>)> {
        let fun = cond.as_scalar_function()?;
        if fun.func_name.l != ast::EQ {
            return None;
        }
        let l_col = fun.get_args()[0].as_column()?;
        let r_col = fun.get_args()[1].as_column()?;
        if l_col.get_type().get_collate() == r_col.get_type().get_collate() {
            return self
                .cols_from_outer_and_inner(&Arc::new(l_col.clone()), &Arc::new(r_col.clone()));
        }
        None
    }

    /// deriveConds given `outerCol = innerCol`, derive new expression for specified conditions.
    // go: propOuterJoinConstSolver.deriveConds L1
    fn derive_conds(
        &mut self,
        outer_col: &Column,
        inner_col: &Arc<Column>,
        schema: &Schema,
        f_conds_offset: usize,
        visited: &mut [bool],
        filter_conds: bool,
    ) {
        let (conds, offset, conds_len) = if filter_conds {
            (
                self.filter_conds.clone(),
                f_conds_offset,
                self.filter_conds.len(),
            )
        } else {
            (self.join_conds.clone(), 0, f_conds_offset)
        };
        for k in 0..conds_len {
            if visited[k + offset] {
                // condition has been used to retrieve equality relation or contains column beyond children schema.
                continue;
            }
            let cond = &conds[k];
            if !expr_from_schema(cond, schema) {
                visited[k + offset] = true;
                continue;
            }
            let (replaced, _, new_expr) =
                try_to_replace_cond(self.base.bctx(), outer_col, inner_col, cond, true);
            if replaced {
                self.join_conds.push(new_expr);
            }
        }
    }

    /// propagateColumnEQ propagates expressions like 'outerCol = innerCol' by adding extra
    /// filters 'expression(..., innerCol, ...)' derived from 'expression(..., outerCol, ...)'
    /// as long as 'expression(..., outerCol, ...)' does not reference columns outside
    /// children schemas of join node. Derived new expressions must be appended into join
    /// condition, not filter condition.
    // go: propOuterJoinConstSolver.propagateColumnEQ L1
    fn propagate_column_eq(&mut self) {
        if self.null_sensitive {
            return;
        }
        let mut visited = vec![false; 2 * self.join_conds.len() + self.filter_conds.len()];
        self.base.union_set = new_int_set(self.base.columns.len());
        // Only consider column equal condition in joinConds.
        // If we have column equal in filter condition, the outer join should have been simplified already.
        for i in 0..self.join_conds.len() {
            let cond = Arc::clone(&self.join_conds[i]);
            if let Some((outer_col, inner_col)) = self.valid_col_equal_cond(&cond) {
                let outer_id = self.base.get_col_id(&outer_col);
                let inner_id = self.base.get_col_id(&inner_col);
                self.base.union_set.union(outer_id, inner_id);
                visited[i] = true;
                // Generate `innerCol is not null` from `outerCol = innerCol`. Note that `outerCol is not null`
                // does not hold since we are in outer join.
                // For AntiLeftOuterSemiJoin, this does not work, for example:
                // `select *, t1.a not in (select t2.b from t t2) from t t1` does not imply `t2.b is not null`.
                // For LeftOuterSemiJoin, this does not work either, for example:
                // `select *, t1.a in (select t2.b from t t2) from t t1`
                // rows with t2.b is null would impact whether LeftOuterSemiJoin should output 0 or null if there
                // is no row satisfying t2.b = t1.a
                let child_col = self
                    .inner_schema
                    .retrieve_column(&inner_col)
                    .expect("the inner column is in the inner schema");
                if !mysql::has_not_null_flag(child_col.ret_type.get_flag()) {
                    let child = Expr::Column((**child_col).clone()).into_ref();
                    if let Some(not_null_expr) = build_not_null_expr(self.base.bctx(), &child) {
                        self.join_conds.push(not_null_expr);
                    }
                }
            }
        }
        let len_join_conds = self.join_conds.len();
        let merged_schema = merge_schema(Some(self.outer_schema), Some(self.inner_schema))
            .expect("both schemas are given");
        let n = self.base.columns.len();
        for i in 0..n {
            for j in i + 1..n {
                // unionSet doesn't have iterate(), we use a two layer loop to iterate col_i = col_j relation.
                if self.base.union_set.find_root(i) != self.base.union_set.find_root(j) {
                    continue;
                }
                let coli = Arc::clone(&self.base.columns[i]);
                let colj = Arc::clone(&self.base.columns[j]);
                let Some((outer_col, inner_col)) = self.cols_from_outer_and_inner(&coli, &colj)
                else {
                    continue;
                };
                self.derive_conds(
                    &outer_col,
                    &inner_col,
                    &merged_schema,
                    len_join_conds,
                    &mut visited,
                    false,
                );
                self.derive_conds(
                    &outer_col,
                    &inner_col,
                    &merged_schema,
                    len_join_conds,
                    &mut visited,
                    true,
                );
            }
        }
    }

    // go: propOuterJoinConstSolver.solve L1
    fn solve(
        &mut self,
        join_conds: &[ExprRef],
        filter_conds: &[ExprRef],
    ) -> (Vec<ExprRef>, Vec<ExprRef>) {
        let mut cols = Vec::with_capacity(join_conds.len() + filter_conds.len());
        for cond in join_conds {
            self.join_conds.extend(split_cnf_items(cond));
            cols.extend(extract_columns(cond));
        }
        for cond in filter_conds {
            self.filter_conds.extend(split_cnf_items(cond));
            cols.extend(extract_columns(cond));
        }
        for col in &cols {
            self.base.insert_col(col);
        }
        if self.base.columns.len() > MaxPropagateColsCnt {
            // Go logs "too many columns" at warn level.
            return (join_conds.to_vec(), filter_conds.to_vec());
        }
        self.propagate_constant_eq();
        self.propagate_column_eq();
        let ctx = Arc::clone(self.base.ctx());
        let jc = std::mem::take(&mut self.join_conds);
        let fc = std::mem::take(&mut self.filter_conds);
        (
            propagate_constant_dnf(&ctx, jc),
            propagate_constant_dnf(&ctx, fc),
        )
    }
}

/// propagateConstantDNF find DNF item from CNF, and propagate constant inside DNF.
// go: propagateConstantDNF L1
fn propagate_constant_dnf(ctx: &ExprContextRef, mut conds: Vec<ExprRef>) -> Vec<ExprRef> {
    for cond in conds.iter_mut() {
        if let Expr::ScalarFunction(dnf) = &**cond
            && dnf.func_name.l == ast::LogicOr
        {
            let mut dnf_items = split_dnf_items(cond);
            for item in dnf_items.iter_mut() {
                let items = propagate_constant(ctx, std::slice::from_ref(item));
                // Go stores nil when NewFunctionInternal fails; the item is kept here.
                if let Some(c) = compose_cnf_condition(&**ctx, &items) {
                    *item = c;
                }
            }
            if let Some(c) = compose_dnf_condition(&**ctx, &dnf_items) {
                *cond = c;
            }
        }
    }
    conds
}

/// PropConstOverOuterJoin propagate constant equal and column equal conditions over outer
/// join. First step is to extract `outerCol = const` from join conditions and filter
/// conditions, and substitute `outerCol` in join conditions with `const`; Second step is to
/// extract `outerCol = innerCol` from join conditions, and derive new join conditions based
/// on this column equal condition and `outerCol` related expressions in join conditions and
/// filter conditions;
// go: PropConstOverOuterJoin L1
pub fn prop_const_over_outer_join(
    ctx: &ExprContextRef,
    join_conds: &[ExprRef],
    filter_conds: &[ExprRef],
    outer_schema: &Schema,
    inner_schema: &Schema,
    null_sensitive: bool,
) -> (Vec<ExprRef>, Vec<ExprRef>) {
    let mut solver = PropOuterJoinConstSolver {
        base: BasePropConstSolver::new(Some(Arc::clone(ctx))),
        join_conds: Vec::new(),
        filter_conds: Vec::new(),
        outer_schema,
        inner_schema,
        null_sensitive,
    };
    solver.solve(join_conds, filter_conds)
}
