//! go-file: pkg/planner/core/rule/util/misc.go
//!
//! Column replacement used by `ReplaceExprColumns` and projection elimination (block D09.13;
//! the whole file is ported with VS1-23, spec §2). Go replaces the arguments of a
//! `*ScalarFunction` in place (`expr.GetArgs()[i] = ...`), so a function node shared by
//! several plan nodes changes for all of them. Here the expression is a slot
//! (`&mut ExprRef`) and the change is copy-on-write (`M-EXPR`, M-FT recommendation (a) of the
//! D09.7 pilot): the node is changed in place when this slot is its only owner (what Go
//! does) and copied otherwise; it is copied only when an argument actually changes. The
//! VS1 plan's M-FT audit table lists why no plan this port builds can see the difference.

use std::sync::Arc;

use tidb_expression::{Column, Expr, ExprRef};

use crate::base::ReplaceMap;

/// ResolveExprAndReplace replaces columns fields of expressions by children logical plans.
// go: ResolveExprAndReplace L1 [M-EXPR]
pub fn resolve_expr_and_replace(origin: &mut ExprRef, replace: &ReplaceMap) {
    match &**origin {
        Expr::Column(expr) => {
            if let Some(new_col) = resolve_column_and_replace_impl(expr, replace) {
                *origin = Expr::Column(new_col).into_ref();
            }
        }
        Expr::CorrelatedColumn(expr) => {
            let Some(new_col) = resolve_column_and_replace_impl(&expr.column, replace) else {
                return;
            };
            // newExpr := expr.Clone(); newExpr.Data = expr.Data (the slot travels with the
            // clone, N4); newExpr.Column = *newCol.
            let mut new_expr = expr.clone();
            new_expr.column = new_col;
            *origin = Expr::CorrelatedColumn(new_expr).into_ref();
        }
        Expr::ScalarFunction(expr) => {
            let mut new_args = expr.get_args().to_vec();
            let mut changed = false;
            for arg in new_args.iter_mut() {
                let before = Arc::clone(arg);
                resolve_expr_and_replace(arg, replace);
                changed |= !Arc::ptr_eq(&before, arg);
            }
            if changed && let Expr::ScalarFunction(f) = Arc::make_mut(origin) {
                // The cached hash code is kept, as in Go (which does not reset it either).
                f.function.args = new_args;
            }
        }
        Expr::Constant(_) => {}
    }
}

/// ResolveColumnAndReplace replaces columns fields of expressions by children logical plans.
// go: ResolveColumnAndReplace L1 [M-EXPR]
pub fn resolve_column_and_replace(origin: &Arc<Column>, replace: &ReplaceMap) -> Arc<Column> {
    match resolve_column_and_replace_impl(origin, replace) {
        Some(new_col) => Arc::new(new_col),
        None => Arc::clone(origin),
    }
}

/// resolveColumnAndReplace: `Some(new column)` when the column is replaced (Go's
/// `(newCol, true)`).
// go: resolveColumnAndReplace L1 [M-EXPR]
fn resolve_column_and_replace_impl(origin: &Column, replace: &ReplaceMap) -> Option<Column> {
    let dst = replace.get(origin.hash_code())?;
    // To avoid origin column is shared by multiple operators,
    // need to clone it before modification.
    let mut new_col = (**dst).clone();
    new_col.ret_type = Arc::clone(&origin.ret_type);
    new_col.in_operand = origin.in_operand;
    Some(new_col)
}
