//! go-file: pkg/planner/core/rule_predicate_push_down.go
//!
//! The predicate push down rule (block D09.8) and the `tidb_shard()` prefix it adds for
//! shard indexes.

use std::sync::Arc;

use tidb_base::Error;
use tidb_expression::scalar_function::ScalarFunction;
use tidb_expression::{
    Column, Expr, ExprRef, compose_cnf_condition, compose_dnf_condition, flatten_cnf_conditions,
    flatten_dnf_conditions,
};
use tidb_parser::ast::funcs as ast;

use crate::arena::PlanId;
use crate::base::{self, LogicalOptRule, LogicalPlan};
use crate::logicalop::DataSource;
use crate::planctx::PlanCtx;
use crate::ranger::detacher::add_expr4_eq_and_in_condition;
use crate::util::AccessPath;
use crate::util::column::index_info2_prefix_cols;
use crate::util::optimizetrace::LogicalOptimizeOp;

/// PPDSolver stands for Predicate Push Down.
pub struct PPDSolver;

/// exprPrefixAdder is the wrapper struct to add tidb_shard(x) = val for `OrigConds`
/// `cols` is the index columns for a unique shard index
struct ExprPrefixAdder<'a, 'c> {
    sctx: &'a PlanCtx<'c>,
    orig_conds: Vec<ExprRef>,
    cols: Vec<Arc<Column>>,
    #[allow(dead_code)] // Go stores it and never reads it
    lengths: Vec<isize>,
}

impl LogicalOptRule for PPDSolver {
    /// Optimize implements base.LogicalOptRule.<0th> interface.
    // go: PPDSolver.Optimize L1
    fn optimize(
        &self,
        ctx: &mut PlanCtx<'_>,
        lp: PlanId,
        opt: &LogicalOptimizeOp,
    ) -> Result<(PlanId, bool), Error> {
        let plan_changed = false;
        let (_, p) = base::predicate_push_down(ctx, lp, Vec::new(), opt)?;
        Ok((p, plan_changed))
    }

    /// Name implements base.LogicalOptRule.<1st> interface.
    // go: PPDSolver.Name L1
    fn name(&self) -> &'static str {
        "predicate_push_down"
    }
}

/// addPrefix4ShardIndexes add expression prefix for shard index. e.g. an index is
/// test.uk(tidb_shard(a), a).
/// DataSource.PredicatePushDown ---> DataSource.AddPrefix4ShardIndexes
/// It transforms the sql "SELECT * FROM test WHERE a = 10" to
/// "SELECT * FROM test WHERE tidb_shard(a) = val AND a = 10", val is the value of tidb_shard(10).
/// It also transforms the sql "SELECT * FROM test WHERE a IN (10, 20, 30)" to
/// "SELECT * FROM test WHERE tidb_shard(a) = val1 AND a = 10 OR tidb_shard(a) = val2 AND a = 20"
/// @param[in] conds            the original condtion of this datasource
/// @retval - the new condition after adding expression prefix
// go: addPrefix4ShardIndexes L1 [M-CTX]
pub fn add_prefix4_shard_indexes(
    ds: &DataSource,
    sc: &PlanCtx<'_>,
    conds: Vec<ExprRef>,
) -> Vec<ExprRef> {
    if !ds.contain_expr_prefix_uk {
        return conds;
    }
    let mut new_conds = conds.clone();
    for path in &ds.all_possible_access_paths {
        if !path.is_uk_shard_index_path {
            continue;
        }
        match add_expr_prefix_cond(ds, sc, path, new_conds) {
            Ok(c) => new_conds = c,
            Err(err) => {
                tidb_base::log_error!(
                    "Add tidb_shard expression failed",
                    "error" => &err,
                    "connection id" => sc.vars.connection_id,
                    "database name" => ds.db_name.l.as_str(),
                    "table name" => ds.table_info.name.l.as_str(),
                    "index name" => path.index.as_ref().map(|i| i.name.l.as_str()).unwrap_or(""),
                );
                return conds;
            }
        }
    }
    new_conds
}

// go: addExprPrefixCond L1 [M-CTX]
fn add_expr_prefix_cond(
    ds: &DataSource,
    sc: &PlanCtx<'_>,
    path: &AccessPath,
    conds: Vec<ExprRef>,
) -> Result<Vec<ExprRef>, Error> {
    let col_infos: Vec<_> = ds.columns.iter().map(|c| (**c).clone()).collect();
    let (idx_cols, idx_col_lens) = index_info2_prefix_cols(
        &col_infos,
        &ds.schema(&sc.arena).columns,
        path.index.as_deref().expect("shard index path"),
    );
    if idx_cols.is_empty() {
        return Ok(conds);
    }
    let adder = ExprPrefixAdder {
        sctx: sc,
        orig_conds: conds,
        cols: idx_cols,
        lengths: idx_col_lens,
    };
    adder.add_expr_prefix4_shard_index()
}

impl ExprPrefixAdder<'_, '_> {
    /// AddExprPrefix4ShardIndex
    /// if original condition is a LogicOr expression, such as `WHERE a = 1 OR a = 10`,
    /// call the function AddExprPrefix4DNFCond to add prefix expression tidb_shard(a) = xxx for shard index.
    /// Otherwise, if the condition is  `WHERE a = 1`, `WHERE a = 1 AND b = 10`, `WHERE a IN (1, 2, 3)`......,
    /// call the function AddExprPrefix4CNFCond to add prefix expression for shard index.
    // go: exprPrefixAdder.addExprPrefix4ShardIndex L1
    fn add_expr_prefix4_shard_index(&self) -> Result<Vec<ExprRef>, Error> {
        if self.orig_conds.len() == 1
            && let Expr::ScalarFunction(sf) = &*self.orig_conds[0]
            && sf.func_name.l == ast::LogicOr
        {
            return self.add_expr_prefix4_dnf_cond(&self.orig_conds[0], sf);
        }
        self.add_expr_prefix4_cnf_cond(&self.orig_conds)
    }

    /// AddExprPrefix4CNFCond
    /// add the prefix expression for CNF condition, e.g. `WHERE a = 1`, `WHERE a = 1 AND b = 10`, ......
    // go: exprPrefixAdder.addExprPrefix4CNFCond L1
    fn add_expr_prefix4_cnf_cond(&self, conds: &[ExprRef]) -> Result<Vec<ExprRef>, Error> {
        add_expr4_eq_and_in_condition(&self.sctx.ranger_ctx(), conds, &self.cols)
    }

    /// AddExprPrefix4DNFCond
    /// add the prefix expression for DNF condition, e.g. `WHERE a = 1 OR a = 10`, ......
    /// The condition returned is `WHERE (tidb_shard(a) = 214 AND a = 1) OR (tidb_shard(a) = 142 AND a = 10)`
    // go: exprPrefixAdder.addExprPrefix4DNFCond L1
    fn add_expr_prefix4_dnf_cond(
        &self,
        condition_ref: &ExprRef,
        condition: &ScalarFunction,
    ) -> Result<Vec<ExprRef>, Error> {
        let dnf_items = flatten_dnf_conditions(condition);
        let mut new_access_items = Vec::with_capacity(dnf_items.len());
        let expr_ctx = &*self.sctx.expr_ctx;
        for item in dnf_items {
            if let Expr::ScalarFunction(sf) = &*item {
                if sf.func_name.l == ast::LogicAnd {
                    let cnf_items = flatten_cnf_conditions(sf);
                    let accesses = match self.add_expr_prefix4_cnf_cond(&cnf_items) {
                        Ok(a) => a,
                        Err(err) => return Err(with_orig(err, condition_ref)),
                    };
                    // accesses is never empty: AddExpr4EqAndInCondition returns at least its input.
                    new_access_items.push(
                        compose_cnf_condition(expr_ctx, &accesses).expect("non-empty conditions"),
                    );
                } else if sf.func_name.l == ast::EQ || sf.func_name.l == ast::In {
                    // only add prefix expression for EQ or IN function
                    let accesses = match self.add_expr_prefix4_cnf_cond(std::slice::from_ref(&item))
                    {
                        Ok(a) => a,
                        Err(err) => return Err(with_orig(err, condition_ref)),
                    };
                    // accesses is never empty: AddExpr4EqAndInCondition returns at least its input.
                    new_access_items.push(
                        compose_cnf_condition(expr_ctx, &accesses).expect("non-empty conditions"),
                    );
                } else {
                    new_access_items.push(item);
                }
            } else {
                new_access_items.push(item);
            }
        }
        Ok(vec![
            compose_dnf_condition(expr_ctx, &new_access_items).expect("non-empty DNF items"),
        ])
    }
}

/// Go returns `[]Expression{condition}, err` on error; the only caller drops the
/// conditions when the error is set, so the error alone is returned.
// glue: M-ERRVAL (Go's (value, err) pair whose value the caller ignores on error)
fn with_orig(err: Error, _condition: &ExprRef) -> Error {
    err
}
