//! go-file: pkg/util/ranger/checker.go
//!
//! conditionChecker checks if this condition can be pushed to index planner.

// AST function names and type codes keep their Go names (M-GONAME).
#![allow(non_upper_case_globals)]

use tidb_base::mysql;
use tidb_expression::scalar_function::ast_func as ast;
use tidb_expression::{Column, Expr, ScalarFunction};
use tidb_parser::types::ETString;
use tidb_parser::types::UnspecifiedLength;
use tidb_sessionctx::exprctx::EvalContext;
use tidb_types::collate;

/// conditionChecker checks if this condition can be pushed to index planner.
pub(crate) struct ConditionChecker<'a> {
    pub ctx: &'a dyn EvalContext,
    /// Go's `*expression.Column`, which may be nil.
    pub checker_col: Option<&'a Column>,
    pub length: isize,
    pub opt_prefix_index_single_scan: bool,
}

impl ConditionChecker<'_> {
    // go: conditionChecker.isFullLengthColumn L1
    fn is_full_length_column(&self) -> bool {
        self.length == UnspecifiedLength
            || self.length
                == self
                    .checker_col
                    .expect("checkerCol of a full-length check")
                    .get_type()
                    .get_flen()
    }

    /// check returns two values, isAccessCond and shouldReserve.
    /// isAccessCond indicates whether the condition can be used to build ranges.
    /// shouldReserve indicates whether the condition should be reserved in filter conditions.
    // go: conditionChecker.check L1
    pub fn check(&self, condition: &Expr) -> (bool, bool) {
        match condition {
            Expr::ScalarFunction(x) => return self.check_scalar_function(x),
            Expr::Column(x) => {
                if x.ret_type.eval_type() == ETString {
                    return (false, true);
                }
                return self.check_column(condition);
            }
            Expr::Constant(_) => return (true, false),
            Expr::CorrelatedColumn(_) => {}
        }
        (false, true)
    }

    /// The EQ / NE / ... branch of `checkScalarFunction` for one argument order: `cons` is
    /// the constant side, `col` the column side.
    // glue: the two symmetric branches of checkScalarFunction's comparison case
    fn check_cmp_side(
        &self,
        scalar: &ScalarFunction,
        collation: &str,
        cons: &Expr,
        col: &Expr,
    ) -> Option<(bool, bool)> {
        if !matches!(cons, Expr::Constant(_)) || !self.match_column(col) {
            return None;
        }
        // Checks whether the scalar function is calculated use the collation compatible with the column.
        let col_tp = col.get_type(self.ctx);
        if col_tp.eval_type() == ETString
            && !collate::compatible_collate(col_tp.get_collate(), collation)
        {
            // When comparing a column with a binary-collation constant (e.g. col = CAST(x AS BINARY)),
            // allow building an approximate index range for EQ/NullEQ. The condition is kept as a
            // filter (shouldReserve=true) to ensure the binary comparison semantics are enforced.
            if collate::is_bin_collation(collation)
                && (scalar.func_name.l == ast::EQ || scalar.func_name.l == ast::NullEQ)
            {
                return Some((true, true));
            }
            return Some((false, true));
        }
        let is_full_length = self.is_full_length_column();
        if scalar.func_name.l == ast::NE {
            return Some((is_full_length, !is_full_length));
        }
        Some((true, !is_full_length))
    }

    // go: conditionChecker.checkScalarFunction L1
    fn check_scalar_function(&self, scalar: &ScalarFunction) -> (bool, bool) {
        let (_, collation) = scalar.charset_and_collation();
        let args = scalar.get_args();
        match scalar.func_name.l.as_str() {
            ast::LogicOr | ast::LogicAnd => {
                let (is_access_cond0, should_reserve0) = self.check(&args[0]);
                let (is_access_cond1, should_reserve1) = self.check(&args[1]);
                if is_access_cond0 && is_access_cond1 {
                    return (true, should_reserve0 || should_reserve1);
                }
                return (false, true);
            }
            ast::EQ | ast::NE | ast::GE | ast::GT | ast::LE | ast::LT | ast::NullEQ => {
                if let Some(r) = self.check_cmp_side(scalar, collation, &args[0], &args[1]) {
                    return r;
                }
                if let Some(r) = self.check_cmp_side(scalar, collation, &args[1], &args[0]) {
                    return r;
                }
            }
            ast::IsNull => {
                if self.match_column(&args[0]) {
                    let mut is_null_reserve = false; // We can know whether the column is null from prefix column of any length.
                    if !self.opt_prefix_index_single_scan {
                        is_null_reserve = !self.is_full_length_column();
                    }
                    return (true, is_null_reserve);
                }
                return (false, true);
            }
            ast::IsTruthWithoutNull | ast::IsFalsity | ast::IsTruthWithNull => {
                if let Expr::Column(s) = &*args[0]
                    && s.ret_type.eval_type() == ETString
                {
                    return (false, true);
                }
                return self.check_column(&args[0]);
            }
            ast::UnaryNot => {
                // TODO: support "not like" convert to access conditions.
                let Expr::ScalarFunction(s) = &*args[0] else {
                    // "not column" or "not constant" can't lead to a range.
                    return (false, true);
                };
                if s.func_name.l == ast::Like || s.func_name.l == ast::NullEQ {
                    return (false, true);
                }
                return self.check(&args[0]);
            }
            ast::In => {
                if !self.match_column(&args[0]) {
                    return (false, true);
                }
                let tp0 = args[0].get_type(self.ctx);
                if tp0.eval_type() == ETString
                    && !collate::compatible_collate(tp0.get_collate(), collation)
                {
                    if !collate::is_bin_collation(collation) {
                        return (false, true);
                    }
                    // Binary collation mismatch: verify all IN-list values are constants before
                    // allowing approximate range building with a filter for correctness.
                    for v in &args[1..] {
                        if !matches!(&**v, Expr::Constant(_)) {
                            return (false, true);
                        }
                    }
                    return (true, true);
                }
                for v in &args[1..] {
                    if !matches!(&**v, Expr::Constant(_)) {
                        return (false, true);
                    }
                }
                return (true, !self.is_full_length_column());
            }
            ast::Like => return self.check_like_func(scalar),
            ast::GetParam => {
                // TODO
                return (true, false);
            }
            _ => {}
        }
        (false, true)
    }

    // go: conditionChecker.checkLikeFunc L1
    fn check_like_func(&self, scalar: &ScalarFunction) -> (bool, bool) {
        let (_, collation) = scalar.charset_and_collation();
        let args = scalar.get_args();
        if !collate::compatible_collate(args[0].get_type(self.ctx).get_collate(), collation) {
            return (false, true);
        }
        if !self.match_column(&args[0]) {
            return (false, true);
        }
        let Expr::Constant(pattern) = &*args[1] else {
            return (false, true);
        };
        if pattern.value.is_null() {
            return (false, true);
        }
        let Ok(pattern_str) = pattern.value.to_string() else {
            return (false, true);
        };
        let mut like_func_reserve = !self.is_full_length_column();

        // Different from `=`, trailing spaces are always significant, and can't be ignored in `like`.
        // In tidb's implementation, for PAD SPACE collations, the trailing spaces are removed in the index key. So we are
        // unable to distinguish 'xxx' from 'xxx   ' by a single index range scan, and we may read more data than needed by
        // the `like` function. Therefore, a Selection is needed to filter the data.
        if collate::is_pad_space_collation(collation) {
            like_func_reserve = true;
        }

        if pattern_str.is_empty() {
            return (true, like_func_reserve);
        }
        let Expr::Constant(escape_c) = &*args[2] else {
            // Go: a failed type assertion `.(*expression.Constant)` panics.
            panic!("interface conversion: expression.Expression is not *expression.Constant");
        };
        let escape = escape_c.value.get_int64() as u8;
        let n = pattern_str.len();
        let mut i = 0usize;
        while i < n {
            if pattern_str[i] == escape {
                i += 1;
                if (i as isize) < n as isize - 1 {
                    i += 1;
                    continue;
                }
                break;
            }
            if i == 0 && (pattern_str[i] == b'%' || pattern_str[i] == b'_') {
                return (false, true);
            }
            if pattern_str[i] == b'%' {
                // We currently do not support using `enum like 'xxx%'` to build range
                // see https://github.com/pingcap/tidb/issues/27130 for more details
                if args[0].get_type(self.ctx).get_type() == mysql::TypeEnum {
                    return (false, true);
                }
                if i != n - 1 {
                    like_func_reserve = true;
                }
                break;
            }
            if pattern_str[i] == b'_' {
                // We currently do not support using `enum like 'xxx_'` to build range
                // see https://github.com/pingcap/tidb/issues/27130 for more details
                if args[0].get_type(self.ctx).get_type() == mysql::TypeEnum {
                    return (false, true);
                }
                like_func_reserve = true;
                break;
            }
            i += 1;
        }
        (true, like_func_reserve)
    }

    /// Go's `EqualByExprAndID(ctx, expr)` is false for an expression that is not a column.
    // go: conditionChecker.matchColumn L1
    pub fn match_column(&self, expr: &Expr) -> bool {
        // Check if virtual expression column matched
        if let Some(checker_col) = self.checker_col {
            let Expr::Column(c) = expr else {
                return false;
            };
            return checker_col.equal_by_expr_and_id(self.ctx, c);
        }
        false
    }

    // go: conditionChecker.checkColumn L1
    fn check_column(&self, expr: &Expr) -> (bool, bool) {
        if self.match_column(expr) {
            return (true, !self.is_full_length_column());
        }
        (false, true)
    }
}
