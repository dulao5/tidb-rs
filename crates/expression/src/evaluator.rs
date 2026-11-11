//! go-file: pkg/expression/evaluator.go
//!
//! `EvaluatorSuite`: evaluates a list of expressions into the columns of an output chunk —
//! column expressions by swapping the input columns into place (`ColumnSwapHelper`), the
//! others vectorized or row by row (D08.1 part 2b).
//!
//! The expressions of the default evaluator are compiled for the vector driver when the
//! suite is built (Go calls `expr.VecEvalXxx` on the expression itself; N4 §3.4). `Run`
//! takes the per-execution state ([`EvalState`] for correlated columns, [`RpnState`] for the
//! vector driver's columns) besides the context.

use tidb_base::Error;
use tidb_chunk::Chunk;
use tidb_chunk::chunk_util::ColumnSwapHelper;
use tidb_sessionctx::exprctx::OptionalEvalPropKeySet;

use crate::chunk_executor::{eval_one_cell, eval_one_column, eval_one_vec, vectorizable};
use crate::context::EvalContext;
use crate::eval::{RpnProgram, RpnState};
use crate::expression::{EvalState, Expr, ExprRef};

/// defaultEvaluator: the expressions that are not columns, with their output column indexes.
#[derive(Debug)]
struct DefaultEvaluator {
    output_idxes: Vec<usize>,
    exprs: Vec<ExprRef>,
    /// `exprs[i]` compiled for the vector driver.
    progs: Vec<RpnProgram>,
    vectorizable: bool,
}

impl DefaultEvaluator {
    // go: defaultEvaluator.run L2 (vector evaluation through the compiled RpnProgram)
    fn run(
        &self,
        ctx: &dyn EvalContext,
        st: &EvalState,
        rpn: &mut RpnState,
        vec_enabled: bool,
        input: &Chunk,
        output: &mut Chunk,
    ) -> Result<(), Error> {
        if self.vectorizable {
            for i in 0..self.output_idxes.len() {
                if vec_enabled && self.exprs[i].vectorized() {
                    eval_one_vec(
                        ctx,
                        st,
                        rpn,
                        &self.progs[i],
                        &self.exprs[i],
                        input,
                        output,
                        self.output_idxes[i],
                    )?;
                    continue;
                }

                eval_one_column(ctx, st, &self.exprs[i], input, output, self.output_idxes[i])?;
            }
            return Ok(());
        }

        for r in 0..input.num_rows() {
            let row = input.get_row(r);
            for i in 0..self.output_idxes.len() {
                eval_one_cell(ctx, st, &self.exprs[i], row, output, self.output_idxes[i])?;
            }
        }
        Ok(())
    }

    /// RequiredOptionalEvalProps exposes all optional evaluation properties that this evaluator requires.
    // go: defaultEvaluator.RequiredOptionalEvalProps L1
    fn required_optional_eval_props(&self) -> OptionalEvalPropKeySet {
        let mut props = OptionalEvalPropKeySet(0);
        for expr in &self.exprs {
            props = OptionalEvalPropKeySet(props.0 | get_optional_eval_props_for_expr(expr).0);
        }
        props
    }
}

/// GetOptionalEvalPropsForExpr gets all optional evaluation properties that this expression requires.
// go: GetOptionalEvalPropsForExpr L1
pub fn get_optional_eval_props_for_expr(expr: &Expr) -> OptionalEvalPropKeySet {
    match expr {
        Expr::ScalarFunction(e) => {
            let mut props = e.function.required_optional_eval_props();
            for arg in e.get_args() {
                props = OptionalEvalPropKeySet(props.0 | get_optional_eval_props_for_expr(arg).0);
            }
            props
        }
        _ => OptionalEvalPropKeySet(0),
    }
}

/// EvaluatorSuite is responsible for the evaluation of a list of expressions.
/// It separates them to "column" and "other" expressions and evaluates "other"
/// expressions before "column" expressions.
#[derive(Debug, Default)]
pub struct EvaluatorSuite {
    /// Evaluator for column expressions.
    pub column_swap_helper: Option<ColumnSwapHelper>,
    /// Evaluator for other expressions.
    default_evaluator: Option<DefaultEvaluator>,
}

/// NewEvaluatorSuite creates an EvaluatorSuite to evaluate all the exprs.
/// avoidColumnEvaluator can be removed after column pool is supported.
// go: NewEvaluatorSuite L1
pub fn new_evaluator_suite(exprs: &[ExprRef], avoid_column_evaluator: bool) -> EvaluatorSuite {
    let mut e = EvaluatorSuite::default();

    for (i, expr) in exprs.iter().enumerate() {
        if let (Some(col), false) = (expr.as_column(), avoid_column_evaluator) {
            let helper = e
                .column_swap_helper
                .get_or_insert_with(ColumnSwapHelper::default);
            let (input_idx, output_idx) = (col.index as usize, i);
            helper
                .input_idx_to_output_idxes
                .entry(input_idx)
                .or_default()
                .push(output_idx);
            continue;
        }
        let d = e.default_evaluator.get_or_insert_with(|| DefaultEvaluator {
            output_idxes: Vec::with_capacity(exprs.len()),
            exprs: Vec::with_capacity(exprs.len()),
            progs: Vec::with_capacity(exprs.len()),
            vectorizable: false,
        });
        d.exprs.push(expr.clone());
        d.progs.push(RpnProgram::compile(expr));
        d.output_idxes.push(i);
    }

    if let Some(d) = e.default_evaluator.as_mut() {
        d.vectorizable = vectorizable(&d.exprs);
    }
    e
}

impl EvaluatorSuite {
    /// Vectorizable checks whether this EvaluatorSuite can use vectorizd execution mode.
    // go: EvaluatorSuite.Vectorizable L1
    pub fn vectorizable(&self) -> bool {
        self.default_evaluator
            .as_ref()
            .is_none_or(|d| d.vectorizable)
    }

    /// Run evaluates all the expressions hold by this EvaluatorSuite.
    /// NOTE: "defaultEvaluator" must be evaluated before "columnEvaluator".
    // go: EvaluatorSuite.Run L1
    pub fn run(
        &self,
        ctx: &dyn EvalContext,
        st: &EvalState,
        rpn: &mut RpnState,
        vec_enabled: bool,
        input: &mut Chunk,
        output: &mut Chunk,
    ) -> Result<(), Error> {
        if let Some(d) = &self.default_evaluator {
            d.run(ctx, st, rpn, vec_enabled, input, output)?;
        }

        // NOTE: It should be called after all the other expressions are evaluated
        //	since it will change the content of the input Chunk.
        if let Some(h) = &self.column_swap_helper {
            return h.swap_columns(input, output);
        }
        Ok(())
    }

    /// RequiredOptionalEvalProps exposes all optional evaluation properties that this evaluator requires.
    // go: EvaluatorSuite.RequiredOptionalEvalProps L1
    pub fn required_optional_eval_props(&self) -> OptionalEvalPropKeySet {
        match &self.default_evaluator {
            Some(d) => d.required_optional_eval_props(),
            None => OptionalEvalPropKeySet(0),
        }
    }
}
