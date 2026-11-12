//! go-file: pkg/expression/builtin_compare.go
//! go-file: pkg/expression/builtin_compare_vec.go
//! go-file: pkg/expression/builtin_compare_vec_generated.go
//! go-file: pkg/expression/util.go
//!
//! `builtin_compare.go` (D08.4), first batch: the comparison operators `<`, `<=`, `>`, `>=`,
//! `=`, `!=`, `<=>` (`compareFunctionClass` with its argument refining and the 56 signatures
//! over the eight comparison types), the `CompareXxx` functions and the comparison-type
//! helpers. Batch 2: COALESCE. Batch 3: GREATEST / LEAST (with `ResolveType4Between` and
//! the extremum type resolution) and INTERVAL (specs/D08.4-compare).
#![allow(non_upper_case_globals)]

use std::sync::Arc;

use tidb_base::Error;
use tidb_base::errors::RedactLogDisable;
use tidb_base::mysql::{
    self, TypeDatetime, TypeDouble, TypeDuration, TypeJSON, TypeLonglong, TypeTimestamp,
    TypeUnspecified, TypeYear, has_not_null_flag, has_unsigned_flag,
};
use tidb_chunk::Row;
use tidb_parser::ast::Op;
use tidb_parser::ast::funcs as ast;
use tidb_parser::types::{
    ETDatetime, ETDecimal, ETDuration, ETInt, ETJson, ETReal, ETString, ETTimestamp,
    ETVectorFloat32, EvalType,
};
use tidb_types::collate::get_binary_collator;
use tidb_types::compare::compare_int as types_compare_int;
use tidb_types::convert::{convert_uint_to_int, integer_signed_upper_bound};
use tidb_types::datum::{Datum, cmp_f64};
use tidb_types::errors::ErrOverflow;
use tidb_types::etc::{FieldType, is_temporal_with_date, is_type_time};
use tidb_types::field_type::new_field_type;
use tidb_types::json::{BinaryJSON, compare_binary_json};
use tidb_types::mydecimal::MyDecimal;
use tidb_types::time::{Duration, Time, adjust_year_checked};
use tidb_types::vector::VectorFloat32;

use crate::builtin::{BaseFunctionClass, new_base_builtin_func_with_tp};
use crate::builtin_cast::wrap_with_cast_as_duration;
use crate::builtin_registry::FunctionClass;
use crate::collation::check_and_derive_collation_from_exprs;
use crate::constant::{Constant, new_one, new_zero};
use crate::constant_fold::maybe_over_optimized4_plan_cache;
use crate::context::{BuildContext, EvalContext, type_ctx};
use crate::eval::{Args, BuiltinFunc, SigDesc, SigExtra, SigView};
use crate::expression::{EvalState, Expr, ExprRef};
use crate::scalar_function::new_function_internal;
use crate::util::disable_parse_json_flag4_expr;
use tidb_expression_macros::eval_fn;

// ---------------------------------------------------------------- the opcode tables

/// `symmetricOp` (util.go): the operator with the arguments swapped.
// glue: pkg/expression/util.go symmetricOp (a map variable)
pub fn symmetric_op(op: Op) -> Op {
    match op {
        Op::LT => Op::GT,
        Op::GE => Op::LE,
        Op::GT => Op::LT,
        Op::LE => Op::GE,
        Op::EQ => Op::EQ,
        Op::NE => Op::NE,
        Op::NullEQ => Op::NullEQ,
        _ => panic!("symmetricOp: {op:?} is not a comparison operator"),
    }
}

// ---------------------------------------------------------------- comparison types

/// getBaseCmpType gets the EvalType that the two args will be treated as when comparing.
// go: getBaseCmpType L1
pub fn get_base_cmp_type(
    lhs: EvalType,
    rhs: EvalType,
    lft: Option<&FieldType>,
    rft: Option<&FieldType>,
) -> EvalType {
    let (mut lhs, mut rhs) = (lhs, rhs);
    if let (Some(l), Some(r)) = (lft, rft)
        && (l.get_type() == TypeUnspecified || r.get_type() == TypeUnspecified)
    {
        if l.get_type() == r.get_type() {
            return ETString;
        }
        if l.get_type() == TypeUnspecified {
            lhs = rhs;
        } else {
            rhs = lhs;
        }
    }
    let l_hybrid = lft.is_some_and(|f| f.hybrid());
    let r_hybrid = rft.is_some_and(|f| f.hybrid());
    if lhs.is_string_kind() && rhs.is_string_kind() {
        return ETString;
    } else if (lhs == ETInt || l_hybrid) && (rhs == ETInt || r_hybrid) {
        return ETInt;
    } else if (lhs == ETDecimal && rhs == ETString) || (lhs == ETString && rhs == ETDecimal) {
        return ETReal;
    } else if ((lhs == ETInt || l_hybrid) || lhs == ETDecimal)
        && ((rhs == ETInt || r_hybrid) || rhs == ETDecimal)
    {
        return ETDecimal;
    } else if let (Some(l), Some(r)) = (lft, rft)
        && (is_temporal_with_date(l.get_type()) && r.get_type() == TypeYear
            || l.get_type() == TypeYear && is_temporal_with_date(r.get_type()))
    {
        return ETDatetime;
    }
    ETReal
}

/// GetAccurateCmpType uses a more complex logic to decide the EvalType of the two args when
/// compare with each other than getBaseCmpType does.
// go: GetAccurateCmpType L1
pub fn get_accurate_cmp_type(ctx: &dyn EvalContext, lhs: &Expr, rhs: &Expr) -> EvalType {
    let lhs_field_type = lhs.get_type(ctx);
    let rhs_field_type = rhs.get_type(ctx);
    let (lhs_eval_type, rhs_eval_type) = (lhs_field_type.eval_type(), rhs_field_type.eval_type());
    let mut cmp_type = get_base_cmp_type(
        lhs_eval_type,
        rhs_eval_type,
        Some(&lhs_field_type),
        Some(&rhs_field_type),
    );
    if lhs_eval_type == ETVectorFloat32 || rhs_eval_type == ETVectorFloat32 {
        cmp_type = ETVectorFloat32;
    } else if (lhs_eval_type.is_string_kind() && lhs_field_type.get_type() == TypeJSON)
        || (rhs_eval_type.is_string_kind() && rhs_field_type.get_type() == TypeJSON)
    {
        cmp_type = ETJson;
    } else if cmp_type == ETString
        && (is_type_time(lhs_field_type.get_type()) || is_type_time(rhs_field_type.get_type()))
    {
        // date[time] <cmp> date[time]
        // string <cmp> date[time]
        // compare as time
        if lhs_field_type.get_type() == rhs_field_type.get_type() {
            cmp_type = lhs_field_type.eval_type();
        } else {
            cmp_type = ETDatetime;
        }
    } else if lhs_field_type.get_type() == TypeDuration && rhs_field_type.get_type() == TypeDuration
    {
        // duration <cmp> duration
        // compare as duration
        cmp_type = ETDuration;
    } else if cmp_type == ETReal || cmp_type == ETString {
        let is_lhs_const = matches!(lhs, Expr::Constant(_));
        let is_rhs_const = matches!(rhs, Expr::Constant(_));
        if (lhs_eval_type == ETDecimal
            && !is_lhs_const
            && rhs_eval_type.is_string_kind()
            && is_rhs_const)
            || (rhs_eval_type == ETDecimal
                && !is_rhs_const
                && lhs_eval_type.is_string_kind()
                && is_lhs_const)
        {
            // <non-const decimal expression> <cmp> <const string expression>
            // or
            // <const string expression> <cmp> <non-const decimal expression>
            //
            // Do comparison as decimal rather than float, in order not to lose precision.
            cmp_type = ETDecimal;
        } else if is_temporal_column(ctx, lhs) && is_rhs_const
            || is_temporal_column(ctx, rhs) && is_lhs_const
        {
            // <temporal column> <cmp> <non-temporal constant>
            // or
            // <non-temporal constant> <cmp> <temporal column>
            //
            // Convert the constant to temporal type.
            let col = match lhs {
                Expr::Column(c) => c,
                _ => match rhs {
                    Expr::Column(c) => c,
                    _ => unreachable!("isTemporalColumn"),
                },
            };
            if col.ret_type.get_type() == TypeDuration {
                cmp_type = ETDuration;
            }
        }
    }
    cmp_type
}

/// A `CompareFunc` (Go: a function value; the string one closes over the collation).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CompareFunc {
    /// CompareInt.
    Int,
    /// CompareReal.
    Real,
    /// CompareDecimal.
    Decimal,
    /// genCompareString(collation).
    String(String),
    /// CompareDuration.
    Duration,
    /// CompareTime.
    Time,
    /// CompareJSON.
    Json,
    /// CompareVectorFloat32.
    VectorFloat32,
}

impl CompareFunc {
    /// Calls the function: `(cmp, isNull)`.
    // glue: the call of a CompareFunc value (an enum instead of a Go function value)
    pub fn call(
        &self,
        ctx: &dyn EvalContext,
        st: &EvalState,
        lhs: &Expr,
        rhs: &Expr,
        lhs_row: Row<'_>,
        rhs_row: Row<'_>,
    ) -> Result<(i64, bool), Error> {
        match self {
            CompareFunc::Int => compare_int(ctx, st, lhs, rhs, lhs_row, rhs_row),
            CompareFunc::Real => compare_real(ctx, st, lhs, rhs, lhs_row, rhs_row),
            CompareFunc::Decimal => compare_decimal(ctx, st, lhs, rhs, lhs_row, rhs_row),
            CompareFunc::String(c) => {
                compare_string_with_collation_info(ctx, st, lhs, rhs, lhs_row, rhs_row, c)
            }
            CompareFunc::Duration => compare_duration(ctx, st, lhs, rhs, lhs_row, rhs_row),
            CompareFunc::Time => compare_time(ctx, st, lhs, rhs, lhs_row, rhs_row),
            CompareFunc::Json => compare_json(ctx, st, lhs, rhs, lhs_row, rhs_row),
            CompareFunc::VectorFloat32 => {
                compare_vector_float32(ctx, st, lhs, rhs, lhs_row, rhs_row)
            }
        }
    }
}

/// GetCmpFunction get the compare function according to two arguments.
// go: GetCmpFunction L2 (returns a CompareFunc value; genCompareString is CompareFunc::String)
pub fn get_cmp_function(ctx: &dyn BuildContext, lhs: &ExprRef, rhs: &ExprRef) -> CompareFunc {
    let et = get_accurate_cmp_type(&*ctx.get_eval_ctx(), lhs, rhs);
    match et {
        ETInt => CompareFunc::Int,
        ETReal => CompareFunc::Real,
        ETDecimal => CompareFunc::Decimal,
        ETString => {
            let coll =
                check_and_derive_collation_from_exprs(ctx, "", ETInt, &[lhs.clone(), rhs.clone()])
                    // Go ignores the error and reads `coll.Collation` of the nil result:
                    // a nil-pointer panic (M-PANIC).
                    .unwrap_or_else(|e| {
                        panic!(
                            "GetCmpFunction: nil ExprCollation ({})",
                            String::from_utf8_lossy(&e.error())
                        )
                    });
            CompareFunc::String(coll.collation)
        }
        ETDuration => CompareFunc::Duration,
        ETDatetime | ETTimestamp => CompareFunc::Time,
        ETJson => CompareFunc::Json,
        ETVectorFloat32 => CompareFunc::VectorFloat32,
        _ => panic!("cannot compare with {}", et.to_string()),
    }
}

// go: genCompareString L2 (CompareFunc::String carries the collation)
#[doc(hidden)]
pub fn gen_compare_string(collation: &str) -> CompareFunc {
    CompareFunc::String(collation.to_owned())
}

/// isTemporalColumn checks if a expression is a temporal column, temporal column indicates
/// time column or duration column.
// go: isTemporalColumn L1
pub fn is_temporal_column(ctx: &dyn EvalContext, expr: &Expr) -> bool {
    let ft = expr.get_type(ctx);
    if !matches!(expr, Expr::Column(_)) {
        return false;
    }
    if !is_type_time(ft.get_type()) && ft.get_type() != TypeDuration {
        return false;
    }
    true
}

// ---------------------------------------------------------------- CompareXxx

/// compareNull compares null values based on the following rules.
/// 1. NULL is considered to be equal to NULL
/// 2. NULL is considered to be smaller than a non-NULL value.
///
/// NOTE: (lhsIsNull == true) or (rhsIsNull == true) is required.
// go: compareNull L1
pub fn compare_null(lhs_is_null: bool, rhs_is_null: bool) -> i64 {
    if lhs_is_null && rhs_is_null {
        return 0;
    }
    if lhs_is_null {
        return -1;
    }
    1
}

macro_rules! compare_fn {
    ($(#[$m:meta])* $name:ident, $eval:ident, |$a:ident, $b:ident| $cmp:expr) => {
        $(#[$m])*
        pub fn $name(
            ctx: &dyn EvalContext,
            st: &EvalState,
            lhs_arg: &Expr,
            rhs_arg: &Expr,
            lhs_row: Row<'_>,
            rhs_row: Row<'_>,
        ) -> Result<(i64, bool), Error> {
            let arg0 = lhs_arg.$eval(ctx, st, lhs_row)?;
            let arg1 = rhs_arg.$eval(ctx, st, rhs_row)?;
            match (arg0, arg1) {
                (Some($a), Some($b)) => Ok(($cmp as i64, false)),
                (a0, a1) => Ok((compare_null(a0.is_none(), a1.is_none()), true)),
            }
        }
    };
}

/// CompareInt compares two integers.
// go: CompareInt L1
pub fn compare_int(
    ctx: &dyn EvalContext,
    st: &EvalState,
    lhs_arg: &Expr,
    rhs_arg: &Expr,
    lhs_row: Row<'_>,
    rhs_row: Row<'_>,
) -> Result<(i64, bool), Error> {
    let arg0 = lhs_arg.eval_int(ctx, st, lhs_row)?;
    let arg1 = rhs_arg.eval_int(ctx, st, rhs_row)?;
    // compare null values.
    let (Some(arg0), Some(arg1)) = (arg0, arg1) else {
        return Ok((compare_null(arg0.is_none(), arg1.is_none()), true));
    };
    let is_unsigned0 = has_unsigned_flag(lhs_arg.get_type(ctx).get_flag());
    let is_unsigned1 = has_unsigned_flag(rhs_arg.get_type(ctx).get_flag());
    Ok((
        types_compare_int(arg0, is_unsigned0, arg1, is_unsigned1) as i64,
        false,
    ))
}

/// CompareStringWithCollationInfo compares two strings with the specified collation
/// information.
// go: CompareStringWithCollationInfo L1
pub fn compare_string_with_collation_info(
    ctx: &dyn EvalContext,
    st: &EvalState,
    lhs_arg: &Expr,
    rhs_arg: &Expr,
    lhs_row: Row<'_>,
    rhs_row: Row<'_>,
    collation: &str,
) -> Result<(i64, bool), Error> {
    let arg0 = lhs_arg.eval_string(ctx, st, lhs_row)?;
    let arg1 = rhs_arg.eval_string(ctx, st, rhs_row)?;
    let (Some(arg0), Some(arg1)) = (&arg0, &arg1) else {
        return Ok((compare_null(arg0.is_none(), arg1.is_none()), true));
    };
    Ok((
        tidb_types::compare::compare_string(arg0, arg1, collation) as i64,
        false,
    ))
}

compare_fn!(
    /// CompareReal compares two float-point values.
    // go: CompareReal L1
    compare_real, eval_real, |a, b| cmp_f64(a, b)
);
compare_fn!(
    /// CompareDecimal compares two decimals.
    // go: CompareDecimal L1
    compare_decimal, eval_decimal, |a, b| a.compare(&b)
);
compare_fn!(
    /// CompareTime compares two datetime or timestamps.
    // go: CompareTime L1
    compare_time, eval_time, |a, b| a.compare(&b)
);
compare_fn!(
    /// CompareDuration compares two durations.
    // go: CompareDuration L1
    compare_duration, eval_duration, |a, b| a.compare(b)
);
compare_fn!(
    /// CompareJSON compares two JSONs.
    // go: CompareJSON L1
    compare_json, eval_json, |a, b| compare_binary_json(a, b)
);
compare_fn!(
    /// CompareVectorFloat32 compares two float32 vectors.
    // go: CompareVectorFloat32 L1
    compare_vector_float32, eval_vector_float32, |a, b| a.compare(&b)
);

// ---------------------------------------------------------------- the function class

/// compareFunctionClass.
pub struct CompareFunctionClass {
    /// The embedded baseFunctionClass.
    pub base: BaseFunctionClass,
    /// op.
    pub op: Op,
}

macro_rules! cmp_class {
    ($name:ident, $fname:expr, $op:expr) => {
        #[doc = concat!("The `compareFunctionClass` of `", stringify!($name), "`.")]
        pub static $name: CompareFunctionClass = CompareFunctionClass {
            base: BaseFunctionClass {
                func_name: $fname,
                min_args: 2,
                max_args: 2,
            },
            op: $op,
        };
    };
}

cmp_class!(LT_CLASS, ast::LT, Op::LT);
cmp_class!(LE_CLASS, ast::LE, Op::LE);
cmp_class!(GT_CLASS, ast::GT, Op::GT);
cmp_class!(GE_CLASS, ast::GE, Op::GE);
cmp_class!(EQ_CLASS, ast::EQ, Op::EQ);
cmp_class!(NE_CLASS, ast::NE, Op::NE);
cmp_class!(NULL_EQ_CLASS, ast::NullEQ, Op::NullEQ);

impl CompareFunctionClass {
    // go: compareFunctionClass.getDisplayName L1
    pub fn get_display_name(&self) -> String {
        self.op.literal().0.to_owned()
    }
}

/// tryToConvertConstantInt tries to convert a constant with other type to a int constant.
/// isExceptional indicates whether the 'int column [cmp] const' might be true/false.
/// If isExceptional is true, ExceptionalVal is returned. Or, CorrectVal is returned.
/// CorrectVal: The computed result. If the constant can be converted to int without
/// exception, return the val. Else return 'con'(the input).
/// ExceptionalVal : It is used to get more information to check whether
/// 'int column [cmp] const' is true/false
///
///   If the op == LT,LE,GT,GE and it gets an Overflow when converting, return inf/-inf.
///   If the op == EQ,NullEQ and the constant can never be equal to the int column, return
///   ‘con’(the input, a non-int constant).
// go: tryToConvertConstantInt L1
fn try_to_convert_constant_int(
    ctx: &dyn BuildContext,
    target_field_type: &FieldType,
    con: &Constant,
) -> (Constant, bool) {
    let eval_ctx = ctx.get_eval_ctx();
    if con.get_type(&*eval_ctx).map(|t| t.eval_type()) == Some(ETInt) {
        return (con.clone(), false);
    }
    let Ok(mut dt) = con.eval(&*eval_ctx, &EvalState::new(), Row::default()) else {
        return (con.clone(), false);
    };
    let (dt, r) = dt.convert_to(&type_ctx(&*eval_ctx), target_field_type);
    let refined = |dt: Datum| {
        let mut c = Constant::new(dt, target_field_type.clone());
        c.deferred_expr = con.deferred_expr.clone();
        c.param_marker = con.param_marker;
        c
    };
    match r {
        Err(e) if ErrOverflow.equal(&e) => (refined(dt), true),
        Err(_) => (con.clone(), false),
        Ok(()) => (refined(dt), false),
    }
}

/// RefineComparedConstant changes a non-integer constant argument to its ceiling or floor
/// result by the given op. isExceptional indicates whether the 'int column [cmp] const'
/// might be true/false (see tryToConvertConstantInt).
///
/// The ceiling and floor are `NewFunctionInternal(ctx, ast.Ceil / ast.Floor, ..)`: until the
/// math functions (D08.5) are ported, that returns no constant and the input is returned,
/// which can change the comparison's result (e.g. a YEAR column against `2e0`; spec §4 item 2,
/// tidb-rs #99). No SQL path may build these comparisons before D08.5.
// go: RefineComparedConstant L1
pub fn refine_compared_constant(
    ctx: &dyn BuildContext,
    target_field_type: FieldType,
    con: &Constant,
    op: Op,
) -> (Constant, bool) {
    let eval_ctx = ctx.get_eval_ctx();
    let Ok(mut dt) = con.eval(&*eval_ctx, &EvalState::new(), Row::default()) else {
        return (con.clone(), false);
    };
    let mut target_field_type = target_field_type;
    if target_field_type.get_type() == mysql::TypeBit {
        target_field_type = new_field_type(TypeLonglong);
    }
    let refined = |dt: Datum, tp: &FieldType| {
        let mut c = Constant::new(dt, tp.clone());
        c.deferred_expr = con.deferred_expr.clone();
        c.param_marker = con.param_marker;
        c
    };
    // Disable AllowNegativeToUnsigned to make sure return 0 when underflow happens.
    let ori_type_ctx = type_ctx(&*eval_ctx);
    let new_type_ctx =
        ori_type_ctx.with_flags(ori_type_ctx.flags().with_allow_negative_to_unsigned(false));
    let (int_datum, r) = dt.convert_to(&new_type_ctx, &target_field_type);
    if let Err(e) = r {
        if ErrOverflow.equal(&e) {
            return (refined(int_datum, &target_field_type), true);
        }
        return (con.clone(), false);
    }
    let (c, r) = int_datum.compare(&ori_type_ctx, &con.value, get_binary_collator());
    if r.is_err() {
        return (con.clone(), false);
    }
    if c == 0 {
        return (refined(int_datum, &target_field_type), false);
    }
    drop(eval_ctx);
    match op {
        Op::LT | Op::GE => {
            let result_expr = new_function_internal(
                ctx,
                ast::Ceil,
                new_field_type(TypeUnspecified),
                vec![Expr::Constant(con.clone()).into_ref()],
            );
            if let Some(Expr::Constant(result_con)) = result_expr.as_deref() {
                return try_to_convert_constant_int(ctx, &target_field_type, result_con);
            }
        }
        Op::LE | Op::GT => {
            let result_expr = new_function_internal(
                ctx,
                ast::Floor,
                new_field_type(TypeUnspecified),
                vec![Expr::Constant(con.clone()).into_ref()],
            );
            if let Some(Expr::Constant(result_con)) = result_expr.as_deref() {
                return try_to_convert_constant_int(ctx, &target_field_type, result_con);
            }
        }
        Op::NullEQ | Op::EQ => {
            let eval_ctx = ctx.get_eval_ctx();
            match con.get_type(&*eval_ctx).map(|t| t.eval_type()) {
                // An integer value equal or NULL-safe equal to a float value which contains
                // non-zero decimal digits is definitely false.
                // e.g.,
                //   1. "integer  =  1.1" is definitely false.
                //   2. "integer <=> 1.1" is definitely false.
                Some(ETReal) | Some(ETDecimal) => return (con.clone(), true),
                Some(ETString) => {
                    // We try to convert the string constant to double.
                    // If the double result equals the int result, we can return the int
                    // result; otherwise, the compare function will be false.
                    let (double_datum, r) =
                        dt.convert_to(&type_ctx(&*eval_ctx), &new_field_type(TypeDouble));
                    if r.is_err() {
                        return (con.clone(), false);
                    }
                    let f = double_datum.get_float64();
                    if f != f.trunc() {
                        return (con.clone(), true);
                    }
                    return (refined(int_datum, &target_field_type), false);
                }
                _ => {}
            }
        }
        _ => {}
    }
    (con.clone(), false)
}

// go: matchRefineRule3Pattern L1
fn match_refine_rule3_pattern(con_eval_type: EvalType, expr_type: &FieldType) -> bool {
    (expr_type.get_type() == TypeTimestamp || expr_type.get_type() == TypeDatetime)
        && (con_eval_type == ETReal || con_eval_type == ETDecimal || con_eval_type == ETInt)
}

/// RemoveMutableConst used to remove the `ParamMarker` and `DeferredExpr` in the `Constant`
/// expr.
///
/// Go changes the constants in place; here a shared node is copied first (M-EXPR).
// go: pkg/expression/util.go RemoveMutableConst L2 (shared nodes are copied before the change)
pub fn remove_mutable_const(ctx: &dyn BuildContext, exprs: &mut [ExprRef]) -> Result<(), Error> {
    for expr in exprs.iter_mut() {
        let needs = match &**expr {
            Expr::Constant(v) => v.param_marker.is_some() || v.deferred_expr.is_some(),
            Expr::ScalarFunction(_) => true,
            _ => false,
        };
        if !needs {
            continue;
        }
        match Arc::make_mut(expr) {
            Expr::Constant(v) => {
                v.param_marker = None;
                if let Some(de) = v.deferred_expr.take() {
                    // evaluate and update v.Value to convert v to a complete immutable
                    // constant.
                    v.value = de.eval(&*ctx.get_eval_ctx(), &EvalState::new(), Row::default())?;
                }
            }
            Expr::ScalarFunction(sf) => {
                remove_mutable_const(ctx, &mut sf.function.args)?;
            }
            _ => {}
        }
    }
    Ok(())
}

impl CompareFunctionClass {
    /// handleDurationTypeComparisonForNullEq handles comparisons between a duration type
    /// column and a non-duration type constant. If the constant cannot be cast to a duration
    /// type and the comparison operator is `<=>`, the expression is rewritten as `0 <=> 1`
    /// (MySQL compatibility; see the Go comment).
    // go: compareFunctionClass.handleDurationTypeComparisonForNullEq L1
    fn handle_duration_type_comparison_for_null_eq(
        &self,
        ctx: &dyn BuildContext,
        arg0: &ExprRef,
        arg1: &ExprRef,
    ) -> Result<Option<Vec<ExprRef>>, Error> {
        // check if a constant value becomes null after being cast to a duration type.
        let cast_to_duration_is_null = |arg: &ExprRef| -> Result<bool, Error> {
            let f = wrap_with_cast_as_duration(ctx, arg.clone())?;
            let v = f.eval_duration(&*ctx.get_eval_ctx(), &EvalState::new(), Row::default())?;
            Ok(v.is_none())
        };
        let arg0_const = arg0.as_constant();
        let arg1_const = arg1.as_constant();
        let ty = |e: &ExprRef| e.get_type(&*ctx.get_eval_ctx()).get_type();
        let mut is_null = false;
        if let Some(c0) = arg0_const
            && c0.deferred_expr.is_none()
            && arg1_const.is_none()
            && ty(arg1) == TypeDuration
        {
            if c0.value.is_null() {
                // This is a const null, there is no need to re-write the expression
                return Ok(None);
            }
            is_null = cast_to_duration_is_null(arg0)?;
        } else if let Some(c1) = arg1_const
            && c1.deferred_expr.is_none()
            && arg0_const.is_none()
            && ty(arg0) == TypeDuration
        {
            if c1.value.is_null() {
                // This is a const null, there is no need to re-write the expression
                return Ok(None);
            }
            is_null = cast_to_duration_is_null(arg1)?;
        }
        if is_null {
            return Ok(Some(vec![
                Expr::Constant(new_zero()).into_ref(),
                Expr::Constant(new_one()).into_ref(),
            ]));
        }
        Ok(None)
    }
}

/// Since the argument refining of cmp functions can bring some risks to the plan-cache, the
/// optimizer needs to decide to whether to skip the refining or skip plan-cache for safety.
/// For example, `unsigned_int_col > ?(-1)` can be refined to `True`, but the validation of
/// this result can be broken if the parameter changes to 1 after.
// go: allowCmpArgsRefining4PlanCache L1
fn allow_cmp_args_refining4_plan_cache(ctx: &dyn BuildContext, args: &[ExprRef]) -> bool {
    if !maybe_over_optimized4_plan_cache(ctx, args) {
        return true; // plan-cache disabled or no parameter in these args
    }
    let eval_ctx = ctx.get_eval_ctx();
    let str_of = |e: &ExprRef| {
        String::from_utf8_lossy(&e.string_with_ctx(&*eval_ctx, RedactLogDisable)).into_owned()
    };

    // For these 3 cases below, we apply the refining:
    // 1. year-expr <cmp> const
    // 2. int-expr <cmp> string/float/double/decimal-const
    // 3. datetime/timestamp column <cmp> int/float/double/decimal-const
    for con_idx in 0..2 {
        if !matches!(&*args[con_idx], Expr::Constant(_)) {
            continue; // not a constant
        }

        // case 1: year-expr <cmp> const
        // refine `year < 12` to `year < 2012` to guarantee the correctness.
        // see https://github.com/pingcap/tidb/issues/41626 for more details.
        let expr_type = args[1 - con_idx].get_type(&*eval_ctx).into_owned();
        let expr_eval_type = expr_type.eval_type();
        if expr_type.get_type() == TypeYear {
            ctx.set_skip_plan_cache(&format!(
                "'{}' may be converted to INT",
                str_of(&args[con_idx])
            ));
            return true;
        }

        // case 2: int-expr <cmp> string/float/double/decimal-const
        // refine `int_key < 1.1` to `int_key < 2` to generate RangeScan instead of FullScan.
        let con_eval_type = args[con_idx].get_type(&*eval_ctx).eval_type();
        if expr_eval_type == ETInt
            && (con_eval_type == ETString || con_eval_type == ETReal || con_eval_type == ETDecimal)
        {
            ctx.set_skip_plan_cache(&format!(
                "'{}' may be converted to INT",
                str_of(&args[con_idx])
            ));
            return true;
        }

        // case 3: datetime/timestamp column <cmp> int/float/double/decimal-const
        // try refine numeric-const to timestamp const
        // see https://github.com/pingcap/tidb/issues/38361 for more details
        let expr_is_con = matches!(&*args[1 - con_idx], Expr::Constant(_));
        if !expr_is_con && match_refine_rule3_pattern(con_eval_type, &expr_type) {
            ctx.set_skip_plan_cache(&format!(
                "'{}' may be converted to datetime",
                str_of(&args[con_idx])
            ));
            return true;
        }
    }
    false
}

impl CompareFunctionClass {
    /// refineArgs will rewrite the arguments if the compare expression is
    ///  1. `int column <cmp> non-int constant` or `non-int constant <cmp> int column`. E.g.,
    ///     `a < 1.1` will be rewritten to `a < 2`.
    ///  2. It also handles comparing year type with int constant if the int constant falls
    ///     into a sensible year representation.
    ///  3. It also handles comparing datetime/timestamp column with numeric constant, try to
    ///     cast numeric constant as timestamp type, do nothing if failed.
    ///  4. Handles special cases where a duration type column is compared with a non-duration
    ///     type constant (`0 <=> 1`, see handleDurationTypeComparisonForNullEq).
    ///
    /// Go changes the YEAR constant in place (the caller's node); here a copy is changed.
    // go: compareFunctionClass.refineArgs L2 (the YEAR constant is copied before it changes, spec §4)
    fn refine_args(
        &self,
        ctx: &dyn BuildContext,
        mut args: Vec<ExprRef>,
    ) -> Result<Vec<ExprRef>, Error> {
        let (arg0_type, arg1_type) = {
            let ev = ctx.get_eval_ctx();
            (
                args[0].get_type(&*ev).into_owned(),
                args[1].get_type(&*ev).into_owned(),
            )
        };
        let (arg0_eval_type, arg1_eval_type) = (arg0_type.eval_type(), arg1_type.eval_type());
        let arg0_is_int = arg0_eval_type == ETInt;
        let arg1_is_int = arg1_eval_type == ETInt;
        let arg0_is_con = args[0].as_constant().is_some();
        let arg1_is_con = args[1].as_constant().is_some();
        let mut is_exceptional = false;
        let (mut is_positive_infinite, mut is_negative_infinite) = (false, false);

        if !allow_cmp_args_refining4_plan_cache(ctx, &args) {
            return Ok(args);
        }
        // We should remove the mutable constant for correctness, because its value may be
        // changed.
        remove_mutable_const(ctx, &mut args)?;
        let (mut final_arg0, mut final_arg1) = (args[0].clone(), args[1].clone());

        // Handle comparison between a duration type column and a non-duration type constant.
        if self.op == Op::NullEQ
            && let Some(result) =
                self.handle_duration_type_comparison_for_null_eq(ctx, &args[0], &args[1])?
        {
            return Ok(result);
        }

        if arg0_is_con && !arg1_is_con && match_refine_rule3_pattern(arg0_eval_type, &arg1_type) {
            return Ok(self.refine_numeric_constant_cmp_datetime(ctx, args, 0));
        }

        if !arg0_is_con && arg1_is_con && match_refine_rule3_pattern(arg1_eval_type, &arg0_type) {
            return Ok(self.refine_numeric_constant_cmp_datetime(ctx, args, 1));
        }

        let mut arg0 = args[0].as_constant().cloned();
        let mut arg1 = args[1].as_constant().cloned();

        // int non-constant [cmp] non-int constant
        if arg0_is_int && !arg0_is_con && !arg1_is_int && arg1_is_con {
            let (c, ex) =
                refine_compared_constant(ctx, arg0_type.clone(), arg1.as_ref().unwrap(), self.op);
            is_exceptional = ex;
            // Why check not null flag
            // eg: int_col > const_val(which is less than min_int32)
            // If int_col got null, compare result cannot be true
            if !is_exceptional || (is_exceptional && has_not_null_flag(arg0_type.get_flag())) {
                final_arg1 = Expr::Constant(c.clone()).into_ref();
            }
            // TODO if the plan doesn't care about whether the result of the function is null
            // or false, we don't need to check the NotNullFlag, then more optimizations can be
            // enabled.
            is_exceptional = is_exceptional && has_not_null_flag(arg0_type.get_flag());
            if is_exceptional
                && c.get_type(&*ctx.get_eval_ctx()).map(|t| t.eval_type()) == Some(ETInt)
            {
                // Judge it is inf or -inf
                // For int:
                //			inf:  01111111 & 1 == 1
                //		   -inf:  10000000 & 1 == 0
                // For uint:
                //			inf:  11111111 & 1 == 1
                //		   -inf:  00000000 & 1 == 0
                if c.value.get_int64() & 1 == 1 {
                    is_positive_infinite = true;
                } else {
                    is_negative_infinite = true;
                }
            }
            arg1 = Some(c);
        }
        // non-int constant [cmp] int non-constant
        if arg1_is_int && !arg1_is_con && !arg0_is_int && arg0_is_con {
            let (c, ex) = refine_compared_constant(
                ctx,
                arg1_type.clone(),
                arg0.as_ref().unwrap(),
                symmetric_op(self.op),
            );
            is_exceptional = ex;
            if !is_exceptional || (is_exceptional && has_not_null_flag(arg1_type.get_flag())) {
                final_arg0 = Expr::Constant(c.clone()).into_ref();
            }
            // TODO if the plan doesn't care about whether the result of the function is null
            // or false, we don't need to check the NotNullFlag, then more optimizations can be
            // enabled.
            is_exceptional = is_exceptional && has_not_null_flag(arg1_type.get_flag());
            if is_exceptional
                && c.get_type(&*ctx.get_eval_ctx()).map(|t| t.eval_type()) == Some(ETInt)
            {
                if c.value.get_int64() & 1 == 1 {
                    is_negative_infinite = true;
                } else {
                    is_positive_infinite = true;
                }
            }
            arg0 = Some(c);
        }

        // int constant [cmp] year type
        if arg0_is_con
            && arg0_is_int
            && arg1_type.get_type() == TypeYear
            && let Some(a0) = arg0.as_mut()
            && !a0.value.is_null()
        {
            let (adjusted, failed) = adjust_year_checked(a0.value.get_int64(), false);
            if failed.is_ok() {
                a0.value.set_int64(adjusted);
                final_arg0 = Expr::Constant(a0.clone()).into_ref();
            }
        }
        // year type [cmp] int constant
        if arg1_is_con
            && arg1_is_int
            && arg0_type.get_type() == TypeYear
            && let Some(a1) = arg1.as_mut()
            && !a1.value.is_null()
        {
            let (adjusted, failed) = adjust_year_checked(a1.value.get_int64(), false);
            if failed.is_ok() {
                a1.value.set_int64(adjusted);
                final_arg1 = Expr::Constant(a1.clone()).into_ref();
            }
        }
        let zero_one = || {
            vec![
                Expr::Constant(new_zero()).into_ref(),
                Expr::Constant(new_one()).into_ref(),
            ]
        };
        if is_exceptional && (self.op == Op::EQ || self.op == Op::NullEQ) {
            // This will always be false.
            return Ok(zero_one());
        }
        if is_positive_infinite {
            // If the op is opcode.LT, opcode.LE
            // This will always be true.
            // If the op is opcode.GT, opcode.GE
            // This will always be false.
            return Ok(zero_one());
        }
        if is_negative_infinite {
            // If the op is opcode.GT, opcode.GE
            // This will always be true.
            // If the op is opcode.LT, opcode.LE
            // This will always be false.
            return Ok(vec![
                Expr::Constant(new_one()).into_ref(),
                Expr::Constant(new_zero()).into_ref(),
            ]);
        }

        Ok(self.refine_args_by_unsigned_flag(ctx, vec![final_arg0, final_arg1]))
    }

    /// see https://github.com/pingcap/tidb/issues/38361 for more details
    // go: compareFunctionClass.refineNumericConstantCmpDatetime L1
    fn refine_numeric_constant_cmp_datetime(
        &self,
        ctx: &dyn BuildContext,
        mut args: Vec<ExprRef>,
        const_arg_idx: usize,
    ) -> Vec<ExprRef> {
        let eval_ctx = ctx.get_eval_ctx();
        let const_arg = args[const_arg_idx].as_constant().unwrap();
        let Ok(mut dt) = const_arg.eval(&*eval_ctx, &EvalState::new(), Row::default()) else {
            return args;
        };
        if dt.is_null() {
            return args;
        }
        let target_field_type = new_field_type(TypeDatetime);
        let (datetime_datum, r) = dt.convert_to(&type_ctx(&*eval_ctx), &target_field_type);
        if r.is_err() || datetime_datum.is_null() {
            return args;
        }
        args[const_arg_idx] =
            Expr::Constant(Constant::new(datetime_datum, target_field_type)).into_ref();
        args
    }

    // go: compareFunctionClass.refineArgsByUnsignedFlag L1
    fn refine_args_by_unsigned_flag(
        &self,
        ctx: &dyn BuildContext,
        mut args: Vec<ExprRef>,
    ) -> Vec<ExprRef> {
        let eval_ctx = ctx.get_eval_ctx();
        // Only handle int cases, cause MySQL declares that `UNSIGNED` is deprecated for FLOAT,
        // DOUBLE and DECIMAL types, and support for it would be removed in a future version.
        if args[0].get_type(&*eval_ctx).eval_type() != ETInt
            || args[1].get_type(&*eval_ctx).eval_type() != ETInt
        {
            return args;
        }
        for i in 0..2 {
            let con = args[1 - i].as_constant().cloned();
            let col_tp = match &*args[i] {
                Expr::Column(c) => Some(c.ret_type.clone()),
                Expr::CorrelatedColumn(c) => Some(c.column.ret_type.clone()),
                _ => None,
            };
            let (Some(con), Some(col_tp)) = (con, col_tp) else {
                continue;
            };
            let v = match con.eval_int(&*eval_ctx, &EvalState::new(), Row::default()) {
                Ok(Some(v)) if v <= 0 => v,
                _ => return args,
            };
            if has_unsigned_flag(con.ret_type.get_flag()) && !has_unsigned_flag(col_tp.get_flag()) {
                let op = if i == 1 {
                    symmetric_op(self.op)
                } else {
                    self.op
                };
                if (op == Op::EQ && has_not_null_flag(col_tp.get_flag())) || op == Op::NullEQ {
                    let (_, r) = convert_uint_to_int(
                        v as u64,
                        integer_signed_upper_bound(col_tp.get_type()),
                        col_tp.get_type(),
                    );
                    if r.is_err() {
                        args[i] = Expr::Constant(new_one()).into_ref();
                        args[1 - i] = Expr::Constant(new_zero()).into_ref();
                        return args;
                    }
                }
            }
            if has_unsigned_flag(col_tp.get_flag())
                && has_not_null_flag(col_tp.get_flag())
                && !has_unsigned_flag(con.ret_type.get_flag())
            {
                let op = if i == 1 {
                    symmetric_op(self.op)
                } else {
                    self.op
                };
                if v == 0
                    && (op == Op::LE
                        || op == Op::GT
                        || op == Op::NullEQ
                        || op == Op::EQ
                        || op == Op::NE)
                {
                    return args;
                }
                // `unsigned_col < 0` equals to `1 < 0`,
                // `unsigned_col > -1` equals to `1 > 0`,
                // `unsigned_col <= -1` equals to `1 <= 0`,
                // `unsigned_col >= 0` equals to `1 >= 0`,
                // `unsigned_col == -1` equals to `1 == 0`,
                // `unsigned_col != -1` equals to `1 != 0`,
                // `unsigned_col <=> -1` equals to `1 <=> 0`,
                // so we can replace the column argument with `1`, and the other constant
                // argument with `0`.
                args[i] = Expr::Constant(new_one()).into_ref();
                args[1 - i] = Expr::Constant(new_zero()).into_ref();
                return args;
            }
        }
        args
    }

    /// generateCmpSigs generates compare function signatures.
    // go: compareFunctionClass.generateCmpSigs L1
    fn generate_cmp_sigs(
        &self,
        ctx: &dyn BuildContext,
        args: Vec<ExprRef>,
        tp: EvalType,
    ) -> Result<BuiltinFunc, Error> {
        let mut bf =
            new_base_builtin_func_with_tp(ctx, self.base.func_name, args, ETInt, &[tp, tp])?;
        if tp == ETJson {
            // In compare, if we cast string to JSON, we shouldn't parse it.
            let ev = ctx.get_eval_ctx();
            for a in bf.args.iter_mut() {
                disable_parse_json_flag4_expr(&*ev, a);
            }
        }
        bf.tp.set_flen(1);
        let table: &[&'static SigDesc; 7] = match tp {
            ETInt => &INT_SIGS,
            ETReal => &REAL_SIGS,
            ETDecimal => &DECIMAL_SIGS,
            ETString => &STRING_SIGS,
            ETDuration => &DURATION_SIGS,
            ETDatetime | ETTimestamp => &TIME_SIGS,
            ETJson => &JSON_SIGS,
            ETVectorFloat32 => &VECTOR_SIGS,
            _ => {
                return Err(Error::plain(format!(
                    "operator {} is not supported for {}",
                    op_name(self.op),
                    tp.to_string()
                )));
            }
        };
        let i = match self.op {
            Op::LT => 0,
            Op::LE => 1,
            Op::GT => 2,
            Op::GE => 3,
            Op::EQ => 4,
            Op::NE => 5,
            Op::NullEQ => 6,
            _ => unreachable!("compareFunctionClass op"),
        };
        Ok(bf.with_sig(table[i]))
    }
}

/// `opcode.Op.String()` (the `%s` of an operator in Go's error text): the name in `ops`.
// glue: opcode.Op.String for the generateCmpSigs error text
fn op_name(op: Op) -> &'static str {
    match op {
        Op::LT => "lt",
        Op::LE => "le",
        Op::GT => "gt",
        Op::GE => "ge",
        Op::EQ => "eq",
        Op::NE => "ne",
        Op::NullEQ => "nulleq",
        _ => "?",
    }
}

impl FunctionClass for CompareFunctionClass {
    /// getFunction sets compare built-in function signatures for various types.
    // go: compareFunctionClass.getFunction L1
    fn get_function(
        &self,
        ctx: &dyn BuildContext,
        raw_args: Vec<ExprRef>,
    ) -> Result<BuiltinFunc, Error> {
        self.base.verify_args(&raw_args)?;
        let args = self.refine_args(ctx, raw_args)?;
        let cmp_type = get_accurate_cmp_type(&*ctx.get_eval_ctx(), &args[0], &args[1]);
        self.generate_cmp_sigs(ctx, args, cmp_type)
    }
}

// ---------------------------------------------------------------- the signatures

/// A borrowed view of a JSON value (Go passes `BinaryJSON` by value, sharing the bytes).
// glue: borrow of a BinaryJSON for compare_binary_json
#[inline]
fn jb<'a>(j: &'a BinaryJSON<'_>) -> BinaryJSON<'a> {
    BinaryJSON::borrowed(j.type_code, &j.value)
}

// glue: the unsigned flag of argument i (mysql.HasUnsignedFlag(b.args[i].GetType(ctx).GetFlag()))
#[inline]
fn unsigned(f: &impl SigView, i: usize) -> bool {
    has_unsigned_flag(f.arg_type(i).get_flag())
}

/// The unsigned flags of the two arguments of an int comparison: bit 0 = lhs, bit 1 = rhs.
// glue: the flag combination of the checked int comparison bodies (D08.1 spec §3.4)
#[inline]
fn int_flags(f: &impl SigView) -> u8 {
    u8::from(unsigned(f, 0)) | (u8::from(unsigned(f, 1)) << 1)
}

// glue: the error builder of the checked int comparison bodies, which have no error rows
#[cold]
fn int_no_err(_ctx: &dyn EvalContext, _f: &impl SigView, _a: i64, _b: i64) -> Error {
    unreachable!("an int comparison has no error rows")
}

/// The int comparisons: `CompareInt` then `resOfXX`. CompareInt evaluates both arguments
/// before looking at NULL (`eager`); the bodies have no error rows, so the `checked` form
/// (one loop per unsigned-flag combination, D08.1 spec §3.4) applies.
macro_rules! int_cmp {
    ($($fname:ident, $op:tt, [$($go:literal),*]);* $(;)?) => {$(
        $(#[doc = $go])*
        #[eval_fn(Int, eager, checked(nflags = 2, flags = int_flags, err = int_no_err))]
        pub fn $fname<const FL: u8>(arg0: i64, arg1: i64) -> (i64, bool) {
            let v = types_compare_int(arg0, FL & 1 != 0, arg1, FL & 2 != 0);
            (i64::from(v $op 0), false)
        }
    )*};
}

// go: builtinLTIntSig.evalInt L1
// go: builtinLTIntSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinLTIntSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
// go: builtinLTIntSig.vectorized L2 (SigDesc.vectorized)
// go: builtinLEIntSig.evalInt L1
// go: builtinLEIntSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinLEIntSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
// go: builtinLEIntSig.vectorized L2 (SigDesc.vectorized)
// go: builtinGTIntSig.evalInt L1
// go: builtinGTIntSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinGTIntSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
// go: builtinGTIntSig.vectorized L2 (SigDesc.vectorized)
// go: builtinGEIntSig.evalInt L1
// go: builtinGEIntSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinGEIntSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
// go: builtinGEIntSig.vectorized L2 (SigDesc.vectorized)
// go: builtinEQIntSig.evalInt L1
// go: builtinEQIntSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinEQIntSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
// go: builtinEQIntSig.vectorized L2 (SigDesc.vectorized)
// go: builtinNEIntSig.evalInt L1
// go: builtinNEIntSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinNEIntSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
// go: builtinNEIntSig.vectorized L2 (SigDesc.vectorized)
// go: vecCompareInt L2 (the checked vector driver: one loop per unsigned-flag combination)
// go: vecResOfLT L2 (folded into the checked vector driver)
// go: vecResOfLE L2 (folded into the checked vector driver)
// go: vecResOfGT L2 (folded into the checked vector driver)
// go: vecResOfGE L2 (folded into the checked vector driver)
// go: vecResOfEQ L2 (folded into the checked vector driver)
// go: vecResOfNE L2 (folded into the checked vector driver)
// go: resOfLT L2 (the `v < 0` of the bodies; NULL and errors by the driver prologue)
// go: resOfLE L2 (the `v <= 0` of the bodies; NULL and errors by the driver prologue)
// go: resOfGT L2 (the `v > 0` of the bodies; NULL and errors by the driver prologue)
// go: resOfGE L2 (the `v >= 0` of the bodies; NULL and errors by the driver prologue)
// go: resOfEQ L2 (the `v == 0` of the bodies; NULL and errors by the driver prologue)
// go: resOfNE L2 (the `v != 0` of the bodies; NULL and errors by the driver prologue)
int_cmp!(
    lt_int, <, ["`builtinLTIntSig.evalInt` (`resOfLT(CompareInt(..))`)."];
    le_int, <=, ["`builtinLEIntSig.evalInt` (`resOfLE(CompareInt(..))`)."];
    gt_int, >, ["`builtinGTIntSig.evalInt` (`resOfGT(CompareInt(..))`)."];
    ge_int, >=, ["`builtinGEIntSig.evalInt` (`resOfGE(CompareInt(..))`)."];
    eq_int, ==, ["`builtinEQIntSig.evalInt` (`resOfEQ(CompareInt(..))`)."];
    ne_int, !=, ["`builtinNEIntSig.evalInt` (`resOfNE(CompareInt(..))`)."];
);

/// The comparisons of the other types: `CompareXxx` then `resOfXX`, both arguments
/// evaluated first (`eager`).
macro_rules! typed_cmp {
    ($ty:ident, $arg:ty, |$f:ident, $a:ident, $b:ident| $cmp:expr;
     $($fname:ident, $op:tt);* $(;)?) => {$(
        #[eval_fn(Int, eager)]
        pub fn $fname(
            _ctx: &dyn EvalContext,
            $f: &impl SigView,
            $a: $arg,
            $b: $arg,
        ) -> Result<Option<i64>, Error> {
            let v: isize = $cmp;
            Ok(Some(i64::from(v $op 0)))
        }
    )*};
}

// go: builtinLTRealSig.evalInt L1
// go: builtinLTRealSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinLTRealSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
// go: builtinLTRealSig.vectorized L2 (SigDesc.vectorized)
// go: builtinLTDecimalSig.evalInt L1
// go: builtinLTDecimalSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinLTDecimalSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
// go: builtinLTDecimalSig.vectorized L2 (SigDesc.vectorized)
// go: builtinLTStringSig.evalInt L1
// go: builtinLTStringSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinLTStringSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
// go: builtinLTStringSig.vectorized L2 (SigDesc.vectorized)
// go: builtinLTTimeSig.evalInt L1
// go: builtinLTTimeSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinLTTimeSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
// go: builtinLTTimeSig.vectorized L2 (SigDesc.vectorized)
// go: builtinLTDurationSig.evalInt L1
// go: builtinLTDurationSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinLTDurationSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
// go: builtinLTDurationSig.vectorized L2 (SigDesc.vectorized)
// go: builtinLTJSONSig.evalInt L1
// go: builtinLTJSONSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinLTJSONSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
// go: builtinLTJSONSig.vectorized L2 (SigDesc.vectorized)
// go: builtinLERealSig.evalInt L1
// go: builtinLERealSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinLERealSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
// go: builtinLERealSig.vectorized L2 (SigDesc.vectorized)
// go: builtinLEDecimalSig.evalInt L1
// go: builtinLEDecimalSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinLEDecimalSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
// go: builtinLEDecimalSig.vectorized L2 (SigDesc.vectorized)
// go: builtinLEStringSig.evalInt L1
// go: builtinLEStringSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinLEStringSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
// go: builtinLEStringSig.vectorized L2 (SigDesc.vectorized)
// go: builtinLETimeSig.evalInt L1
// go: builtinLETimeSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinLETimeSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
// go: builtinLETimeSig.vectorized L2 (SigDesc.vectorized)
// go: builtinLEDurationSig.evalInt L1
// go: builtinLEDurationSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinLEDurationSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
// go: builtinLEDurationSig.vectorized L2 (SigDesc.vectorized)
// go: builtinLEJSONSig.evalInt L1
// go: builtinLEJSONSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinLEJSONSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
// go: builtinLEJSONSig.vectorized L2 (SigDesc.vectorized)
// go: builtinGTRealSig.evalInt L1
// go: builtinGTRealSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinGTRealSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
// go: builtinGTRealSig.vectorized L2 (SigDesc.vectorized)
// go: builtinGTDecimalSig.evalInt L1
// go: builtinGTDecimalSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinGTDecimalSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
// go: builtinGTDecimalSig.vectorized L2 (SigDesc.vectorized)
// go: builtinGTStringSig.evalInt L1
// go: builtinGTStringSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinGTStringSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
// go: builtinGTStringSig.vectorized L2 (SigDesc.vectorized)
// go: builtinGTTimeSig.evalInt L1
// go: builtinGTTimeSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinGTTimeSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
// go: builtinGTTimeSig.vectorized L2 (SigDesc.vectorized)
// go: builtinGTDurationSig.evalInt L1
// go: builtinGTDurationSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinGTDurationSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
// go: builtinGTDurationSig.vectorized L2 (SigDesc.vectorized)
// go: builtinGTJSONSig.evalInt L1
// go: builtinGTJSONSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinGTJSONSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
// go: builtinGTJSONSig.vectorized L2 (SigDesc.vectorized)
// go: builtinGERealSig.evalInt L1
// go: builtinGERealSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinGERealSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
// go: builtinGERealSig.vectorized L2 (SigDesc.vectorized)
// go: builtinGEDecimalSig.evalInt L1
// go: builtinGEDecimalSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinGEDecimalSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
// go: builtinGEDecimalSig.vectorized L2 (SigDesc.vectorized)
// go: builtinGEStringSig.evalInt L1
// go: builtinGEStringSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinGEStringSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
// go: builtinGEStringSig.vectorized L2 (SigDesc.vectorized)
// go: builtinGETimeSig.evalInt L1
// go: builtinGETimeSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinGETimeSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
// go: builtinGETimeSig.vectorized L2 (SigDesc.vectorized)
// go: builtinGEDurationSig.evalInt L1
// go: builtinGEDurationSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinGEDurationSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
// go: builtinGEDurationSig.vectorized L2 (SigDesc.vectorized)
// go: builtinGEJSONSig.evalInt L1
// go: builtinGEJSONSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinGEJSONSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
// go: builtinGEJSONSig.vectorized L2 (SigDesc.vectorized)
// go: builtinEQRealSig.evalInt L1
// go: builtinEQRealSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinEQRealSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
// go: builtinEQRealSig.vectorized L2 (SigDesc.vectorized)
// go: builtinEQDecimalSig.evalInt L1
// go: builtinEQDecimalSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinEQDecimalSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
// go: builtinEQDecimalSig.vectorized L2 (SigDesc.vectorized)
// go: builtinEQStringSig.evalInt L1
// go: builtinEQStringSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinEQStringSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
// go: builtinEQStringSig.vectorized L2 (SigDesc.vectorized)
// go: builtinEQTimeSig.evalInt L1
// go: builtinEQTimeSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinEQTimeSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
// go: builtinEQTimeSig.vectorized L2 (SigDesc.vectorized)
// go: builtinEQDurationSig.evalInt L1
// go: builtinEQDurationSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinEQDurationSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
// go: builtinEQDurationSig.vectorized L2 (SigDesc.vectorized)
// go: builtinEQJSONSig.evalInt L1
// go: builtinEQJSONSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinEQJSONSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
// go: builtinEQJSONSig.vectorized L2 (SigDesc.vectorized)
// go: builtinNERealSig.evalInt L1
// go: builtinNERealSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinNERealSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
// go: builtinNERealSig.vectorized L2 (SigDesc.vectorized)
// go: builtinNEDecimalSig.evalInt L1
// go: builtinNEDecimalSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinNEDecimalSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
// go: builtinNEDecimalSig.vectorized L2 (SigDesc.vectorized)
// go: builtinNEStringSig.evalInt L1
// go: builtinNEStringSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinNEStringSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
// go: builtinNEStringSig.vectorized L2 (SigDesc.vectorized)
// go: builtinNETimeSig.evalInt L1
// go: builtinNETimeSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinNETimeSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
// go: builtinNETimeSig.vectorized L2 (SigDesc.vectorized)
// go: builtinNEDurationSig.evalInt L1
// go: builtinNEDurationSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinNEDurationSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
// go: builtinNEDurationSig.vectorized L2 (SigDesc.vectorized)
// go: builtinNEJSONSig.evalInt L1
// go: builtinNEJSONSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinNEJSONSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
// go: builtinNEJSONSig.vectorized L2 (SigDesc.vectorized)
typed_cmp!(Real, f64, |_f, a, b| cmp_f64(a, b);
    lt_real, <; le_real, <=; gt_real, >; ge_real, >=; eq_real, ==; ne_real, !=);
typed_cmp!(Decimal, &MyDecimal, |_f, a, b| a.compare(b);
    lt_decimal, <; le_decimal, <=; gt_decimal, >; ge_decimal, >=; eq_decimal, ==; ne_decimal, !=);
typed_cmp!(String, &[u8], |f, a, b| f.collator().compare(a, b);
    lt_string, <; le_string, <=; gt_string, >; ge_string, >=; eq_string, ==; ne_string, !=);
typed_cmp!(Time, Time, |_f, a, b| a.compare(&b);
    lt_time, <; le_time, <=; gt_time, >; ge_time, >=; eq_time, ==; ne_time, !=);
typed_cmp!(Duration, Duration, |_f, a, b| a.compare(b);
    lt_duration, <; le_duration, <=; gt_duration, >; ge_duration, >=; eq_duration, ==; ne_duration, !=);
typed_cmp!(Json, &BinaryJSON<'_>, |_f, a, b| compare_binary_json(jb(a), jb(b));
    lt_json, <; le_json, <=; gt_json, >; ge_json, >=; eq_json, ==; ne_json, !=);

/// The VECTOR comparisons have no vector version in Go (`novec`).
macro_rules! vector_cmp {
    ($($fname:ident, $op:tt);* $(;)?) => {$(
        #[eval_fn(Int, eager, novec)]
        pub fn $fname(
            _ctx: &dyn EvalContext,
            _f: &impl SigView,
            a: &VectorFloat32<'_>,
            b: &VectorFloat32<'_>,
        ) -> Result<Option<i64>, Error> {
            Ok(Some(i64::from(a.compare(b) $op 0)))
        }
    )*};
}

// go: builtinLTVectorFloat32Sig.evalInt L1
// go: builtinLTVectorFloat32Sig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinLEVectorFloat32Sig.evalInt L1
// go: builtinLEVectorFloat32Sig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinGTVectorFloat32Sig.evalInt L1
// go: builtinGTVectorFloat32Sig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinGEVectorFloat32Sig.evalInt L1
// go: builtinGEVectorFloat32Sig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinEQVectorFloat32Sig.evalInt L1
// go: builtinEQVectorFloat32Sig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinNEVectorFloat32Sig.evalInt L1
// go: builtinNEVectorFloat32Sig.Clone L2 (BuiltinFunc::clone_from_base)
vector_cmp!(lt_vector_float32, <; le_vector_float32, <=; gt_vector_float32, >;
    ge_vector_float32, >=; eq_vector_float32, ==; ne_vector_float32, !=);

// go: builtinNullEQIntSig.evalInt L1
// go: builtinNullEQIntSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
// go: builtinNullEQIntSig.vectorized L2 (SigDesc.vectorized)
// go: builtinNullEQIntSig.Clone L2 (BuiltinFunc::clone_from_base)
#[eval_fn(Int, lazy)]
pub fn null_eq_int(
    ctx: &dyn EvalContext,
    f: &impl SigView,
    args: &mut impl Args,
) -> Result<Option<i64>, Error> {
    let arg0 = args.int(ctx, 0)?;
    let arg1 = args.int(ctx, 1)?;
    let (is_unsigned0, is_unsigned1) = (unsigned(f, 0), unsigned(f, 1));
    Ok(Some(match (arg0, arg1) {
        (None, None) => 1,
        (Some(a), Some(b)) => i64::from(types_compare_int(a, is_unsigned0, b, is_unsigned1) == 0),
        _ => 0,
    }))
}

/// The NULL-safe equality of the other types (lazy bodies: both arguments are evaluated and
/// a NULL compares equal to a NULL only).
macro_rules! null_eq {
    ($($(#[$m:meta])* $fname:ident, $get:ident, |$f:ident, $a:ident, $b:ident| $eq:expr);* $(;)?) => {$(
        $(#[$m])*
        #[eval_fn(Int, lazy)]
        pub fn $fname(
            ctx: &dyn EvalContext,
            $f: &impl SigView,
            args: &mut impl Args,
        ) -> Result<Option<i64>, Error> {
            // The vector driver reads a Duration column with fsp 0: a comparison only looks at
            // the nanoseconds, so the result is the same as Go's (spec §4 item 4).
            let arg0 = args.$get(ctx, 0)?.map(own);
            let arg1 = args.$get(ctx, 1)?;
            Ok(Some(match (arg0, arg1) {
                (None, None) => 1,
                (Some($a), Some($b)) => i64::from($eq),
                _ => 0,
            }))
        }
    )*};
}

/// Owns an evaluated value, so a lazy body can hold the first argument while it evaluates
/// the second.
// glue: lifetime helper of the NULL-safe equality bodies
trait Own {
    type Owned;
    // glue: lifetime helper of the NULL-safe equality bodies
    fn own(self) -> Self::Owned;
}
impl Own for f64 {
    type Owned = f64;
    // glue: lifetime helper of the NULL-safe equality bodies
    fn own(self) -> f64 {
        self
    }
}
impl Own for MyDecimal {
    type Owned = MyDecimal;
    // glue: lifetime helper of the NULL-safe equality bodies
    fn own(self) -> MyDecimal {
        self
    }
}
impl Own for std::borrow::Cow<'_, [u8]> {
    type Owned = Vec<u8>;
    // glue: lifetime helper of the NULL-safe equality bodies
    fn own(self) -> Vec<u8> {
        self.into_owned()
    }
}
impl Own for Time {
    type Owned = Time;
    // glue: lifetime helper of the NULL-safe equality bodies
    fn own(self) -> Time {
        self
    }
}
impl Own for Duration {
    type Owned = Duration;
    // glue: lifetime helper of the NULL-safe equality bodies
    fn own(self) -> Duration {
        self
    }
}
impl Own for BinaryJSON<'_> {
    type Owned = BinaryJSON<'static>;
    // glue: lifetime helper of the NULL-safe equality bodies
    fn own(self) -> BinaryJSON<'static> {
        self.into_owned()
    }
}
impl Own for VectorFloat32<'_> {
    type Owned = VectorFloat32<'static>;
    // glue: lifetime helper of the NULL-safe equality bodies
    fn own(self) -> VectorFloat32<'static> {
        VectorFloat32::clone(&self)
    }
}
// glue: lifetime helper of the NULL-safe equality bodies
#[inline]
fn own<T: Own>(v: T) -> T::Owned {
    v.own()
}

null_eq!(
    // go: builtinNullEQRealSig.evalInt L1
    // go: builtinNullEQRealSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
    // go: builtinNullEQRealSig.vectorized L2 (SigDesc.vectorized)
    // go: builtinNullEQRealSig.Clone L2 (BuiltinFunc::clone_from_base)
    null_eq_real, real, |_f, a, b| cmp_f64(a, b) == 0;
    // go: builtinNullEQDecimalSig.evalInt L1
    // go: builtinNullEQDecimalSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
    // go: builtinNullEQDecimalSig.vectorized L2 (SigDesc.vectorized)
    // go: builtinNullEQDecimalSig.Clone L2 (BuiltinFunc::clone_from_base)
    null_eq_decimal, decimal, |_f, a, b| a.compare(&b) == 0;
    // go: builtinNullEQStringSig.evalInt L1
    // go: builtinNullEQStringSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
    // go: builtinNullEQStringSig.vectorized L2 (SigDesc.vectorized)
    // go: builtinNullEQStringSig.Clone L2 (BuiltinFunc::clone_from_base)
    null_eq_string, string, |f, a, b| f.collator().compare(&a, &b) == 0;
    // go: builtinNullEQTimeSig.evalInt L1
    // go: builtinNullEQTimeSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
    // go: builtinNullEQTimeSig.vectorized L2 (SigDesc.vectorized)
    // go: builtinNullEQTimeSig.Clone L2 (BuiltinFunc::clone_from_base)
    null_eq_time, time, |_f, a, b| a.compare(&b) == 0;
    // go: builtinNullEQDurationSig.evalInt L1
    // go: builtinNullEQDurationSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
    // go: builtinNullEQDurationSig.vectorized L2 (SigDesc.vectorized)
    // go: builtinNullEQDurationSig.Clone L2 (BuiltinFunc::clone_from_base)
    null_eq_duration, duration, |_f, a, b| a.compare(b) == 0;
    // go: builtinNullEQJSONSig.evalInt L1
    // go: builtinNullEQJSONSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
    // go: builtinNullEQJSONSig.vectorized L2 (SigDesc.vectorized)
    // go: builtinNullEQJSONSig.Clone L2 (BuiltinFunc::clone_from_base)
    null_eq_json, json, |_f, a, b| compare_binary_json(jb(&a), jb(&b)) == 0;
);

// go: builtinNullEQVectorFloat32Sig.evalInt L1
// go: builtinNullEQVectorFloat32Sig.Clone L2 (BuiltinFunc::clone_from_base)
#[eval_fn(Int, lazy, novec)]
pub fn null_eq_vector_float32(
    ctx: &dyn EvalContext,
    _f: &impl SigView,
    args: &mut impl Args,
) -> Result<Option<i64>, Error> {
    let arg0 = args.vector_float32(ctx, 0)?.map(own);
    let arg1 = args.vector_float32(ctx, 1)?;
    Ok(Some(match (arg0, arg1) {
        (None, None) => 1,
        (Some(a), Some(b)) => i64::from(a.compare(&b) == 0),
        _ => 0,
    }))
}

// ---------------------------------------------------------------- COALESCE

/// coalesceFunctionClass returns the first non-NULL value in the list,
/// or NULL if there are no non-NULL values.
#[derive(Debug)]
pub struct CoalesceFunctionClass {
    /// The embedded baseFunctionClass.
    pub base: BaseFunctionClass,
}

/// The function class of `coalesce`.
pub static COALESCE_CLASS: CoalesceFunctionClass = CoalesceFunctionClass {
    base: BaseFunctionClass {
        func_name: ast::Coalesce,
        min_args: 1,
        max_args: -1,
    },
};

impl FunctionClass for CoalesceFunctionClass {
    // go: coalesceFunctionClass.getFunction L1
    fn get_function(
        &self,
        ctx: &dyn BuildContext,
        args: Vec<ExprRef>,
    ) -> Result<BuiltinFunc, Error> {
        self.base.verify_args(&args)?;

        let mut flag = 0;
        {
            let ectx = ctx.get_eval_ctx();
            for arg in &args {
                flag |= arg.get_type(&*ectx).get_flag() & tidb_base::mysql::NotNullFlag;
            }
        }

        let mut result_field_type =
            crate::builtin_control::infer_type4_control_funcs(ctx, self.base.func_name, &args)?;

        result_field_type.add_flag(flag);

        let ret_eval_tp = result_field_type.eval_type();
        let field_eval_tps = vec![ret_eval_tp; args.len()];

        let mut bf = new_base_builtin_func_with_tp(
            ctx,
            self.base.func_name,
            args,
            ret_eval_tp,
            &field_eval_tps,
        )?;

        bf.tp = result_field_type;

        let sig: &'static SigDesc = match ret_eval_tp {
            ETInt => &builtinCoalesceIntSig,
            ETReal => &builtinCoalesceRealSig,
            ETDecimal => &builtinCoalesceDecimalSig,
            ETString => &builtinCoalesceStringSig,
            // bf.tp.SetDecimal(resultFieldType.GetDecimal()): bf.tp is resultFieldType.
            ETDatetime | ETTimestamp => &builtinCoalesceTimeSig,
            ETDuration => &builtinCoalesceDurationSig,
            ETJson => &builtinCoalesceJSONSig,
            ETVectorFloat32 => &builtinCoalesceVectorFloat32Sig,
            other => {
                return Err(Error::plain(format!(
                    "{} is not supported for COALESCE()",
                    other.to_string()
                )));
            }
        };
        Ok(bf.with_sig(sig))
    }
}

// The bodies are the row versions (`evalXxx`: the first argument that is not NULL, or the
// first error). The vector versions evaluate every argument column themselves and fall
// back to the row version when one of them fails or warns (ArgsFallback::EveryArg); their
// merge ("the first column that is not NULL on this row") is the lazy body over the
// evaluated columns.

/// COALESCE over int arguments.
// go: builtinCoalesceIntSig.evalInt L1
// go: builtinCoalesceIntSig.vecEvalInt L2 (RpnNode::RowFallback with ArgsFallback::EveryArg; the merge is this lazy body's vector driver)
// go: builtinCoalesceIntSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinCoalesceIntSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Int, lazy, args(Int))]
pub fn coalesce_int(
    ctx: &dyn EvalContext,
    _f: &impl SigView,
    args: &mut impl Args,
) -> Result<Option<i64>, Error> {
    for i in 0..args.len() {
        if let Some(v) = args.int(ctx, i)? {
            return Ok(Some(v));
        }
    }
    Ok(None)
}

/// COALESCE over real arguments.
// go: builtinCoalesceRealSig.evalReal L1
// go: builtinCoalesceRealSig.vecEvalReal L2 (RpnNode::RowFallback with ArgsFallback::EveryArg; the merge is this lazy body's vector driver)
// go: builtinCoalesceRealSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinCoalesceRealSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Real, lazy, args(Real))]
pub fn coalesce_real(
    ctx: &dyn EvalContext,
    _f: &impl SigView,
    args: &mut impl Args,
) -> Result<Option<f64>, Error> {
    for i in 0..args.len() {
        if let Some(v) = args.real(ctx, i)? {
            return Ok(Some(v));
        }
    }
    Ok(None)
}

/// COALESCE over decimal arguments.
// go: builtinCoalesceDecimalSig.evalDecimal L1
// go: builtinCoalesceDecimalSig.vecEvalDecimal L2 (RpnNode::RowFallback with ArgsFallback::EveryArg; the merge is this lazy body's vector driver)
// go: builtinCoalesceDecimalSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinCoalesceDecimalSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Decimal, lazy, args(Decimal))]
pub fn coalesce_decimal(
    ctx: &dyn EvalContext,
    _f: &impl SigView,
    args: &mut impl Args,
) -> Result<Option<MyDecimal>, Error> {
    for i in 0..args.len() {
        if let Some(v) = args.decimal(ctx, i)? {
            return Ok(Some(v));
        }
    }
    Ok(None)
}

/// COALESCE over string arguments (Go's vector version evaluates every argument column
/// first and then merges; same result as merging after each).
// go: builtinCoalesceStringSig.evalString L1
// go: builtinCoalesceStringSig.vecEvalString L2 (RpnNode::RowFallback with ArgsFallback::EveryArg; the merge is this lazy body's vector driver)
// go: builtinCoalesceStringSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinCoalesceStringSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(String, lazy, args(String))]
pub fn coalesce_string(
    ctx: &dyn EvalContext,
    _f: &impl SigView,
    args: &mut impl Args,
    out: &mut Vec<u8>,
) -> Result<Option<()>, Error> {
    for i in 0..args.len() {
        if let Some(v) = args.string(ctx, i)? {
            out.extend_from_slice(&v);
            return Ok(Some(()));
        }
    }
    Ok(None)
}

/// COALESCE over time arguments: the value gets the result's fsp (`res.SetFsp(fsp)`).
// go: builtinCoalesceTimeSig.evalTime L1
// go: builtinCoalesceTimeSig.vecEvalTime L2 (RpnNode::RowFallback with ArgsFallback::EveryArg; the merge is this lazy body's vector driver)
// go: builtinCoalesceTimeSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinCoalesceTimeSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Time, lazy, args(Time))]
pub fn coalesce_time(
    ctx: &dyn EvalContext,
    f: &impl SigView,
    args: &mut impl Args,
) -> Result<Option<Time>, Error> {
    let fsp = f.tp.get_decimal();
    for i in 0..args.len() {
        if let Some(mut v) = args.time(ctx, i)? {
            v.set_fsp(fsp);
            return Ok(Some(v));
        }
    }
    Ok(None)
}

/// COALESCE over duration arguments: the value gets the result's fsp (`res.Fsp = ..`).
// go: builtinCoalesceDurationSig.evalDuration L1
// go: builtinCoalesceDurationSig.vecEvalDuration L2 (RpnNode::RowFallback with ArgsFallback::EveryArg; the merge is this lazy body's vector driver)
// go: builtinCoalesceDurationSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinCoalesceDurationSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Duration, lazy, args(Duration))]
pub fn coalesce_duration(
    ctx: &dyn EvalContext,
    f: &impl SigView,
    args: &mut impl Args,
) -> Result<Option<Duration>, Error> {
    let fsp = f.tp.get_decimal();
    for i in 0..args.len() {
        if let Some(mut v) = args.duration(ctx, i)? {
            v.fsp = fsp;
            return Ok(Some(v));
        }
    }
    Ok(None)
}

/// COALESCE over JSON arguments (Go's vector version evaluates every argument column
/// first and then merges, like the string one).
// go: builtinCoalesceJSONSig.evalJSON L1
// go: builtinCoalesceJSONSig.vecEvalJSON L2 (RpnNode::RowFallback with ArgsFallback::EveryArg; the merge is this lazy body's vector driver)
// go: builtinCoalesceJSONSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinCoalesceJSONSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Json, lazy, args(Json))]
pub fn coalesce_json(
    ctx: &dyn EvalContext,
    _f: &impl SigView,
    args: &mut impl Args,
) -> Result<Option<BinaryJSON<'static>>, Error> {
    for i in 0..args.len() {
        if let Some(v) = args.json(ctx, i)? {
            return Ok(Some(v.into_owned()));
        }
    }
    Ok(None)
}

/// COALESCE over vector arguments (no vector version in Go).
// go: builtinCoalesceVectorFloat32Sig.evalVectorFloat32 L1
// go: builtinCoalesceVectorFloat32Sig.Clone L2 (BuiltinFunc::clone_from_base)
#[eval_fn(VectorFloat32, lazy, args(VectorFloat32), novec)]
pub fn coalesce_vector_float32(
    ctx: &dyn EvalContext,
    _f: &impl SigView,
    args: &mut impl Args,
) -> Result<Option<VectorFloat32<'static>>, Error> {
    for i in 0..args.len() {
        if let Some(v) = args.vector_float32(ctx, i)? {
            return Ok(Some(v.clone()));
        }
    }
    Ok(None)
}

// ---------------------------------------------------------------- GREATEST / LEAST / INTERVAL

/// aggregateType: the aggregated field type of the arguments (`types.AggFieldType`).
// go: aggregateType L1
fn aggregate_type(ctx: &dyn EvalContext, args: &[ExprRef]) -> FieldType {
    let field_types: Vec<_> = args.iter().map(|a| a.get_type(ctx)).collect();
    let refs: Vec<&FieldType> = field_types.iter().map(|t| &**t).collect();
    tidb_types::field_type::agg_field_type(&refs)
}

/// ResolveType4Between resolves eval type for between expression.
// go: ResolveType4Between L1
pub fn resolve_type4_between(ctx: &dyn EvalContext, args: [&Expr; 3]) -> EvalType {
    let mut cmp_tp = args[0].get_type(ctx).eval_type();
    for a in &args[1..] {
        cmp_tp = get_base_cmp_type(cmp_tp, a.get_type(ctx).eval_type(), None, None);
    }

    let mut has_temporal = false;
    if cmp_tp == ETString {
        if args[0].get_type(ctx).get_type() == TypeDuration {
            cmp_tp = ETDuration;
        } else {
            for a in &args {
                if tidb_types::etc::is_type_temporal(a.get_type(ctx).get_type()) {
                    has_temporal = true;
                    break;
                }
            }
            if has_temporal {
                cmp_tp = ETDatetime;
            }
        }
    }
    let int_like =
        |a: &Expr| a.get_type(ctx).eval_type() == ETInt || crate::expression::is_binary_literal(a);
    if int_like(args[0]) && int_like(args[1]) && int_like(args[2]) {
        return ETInt;
    }

    cmp_tp
}

/// GLCmpStringMode represents Greatest/Least integral string comparison mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GLCmpStringMode {
    /// Greatest and Least function compares string directly.
    GLCmpStringDirectly,
    /// Greatest/Least function compares string as 'yyyy-mm-dd' format.
    GLCmpStringAsDate,
    /// Greatest/Least function compares string as 'yyyy-mm-dd hh:mm:ss' format.
    GLCmpStringAsDatetime,
}

/// GLRetTimeType represents Greatest/Least return time type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GLRetTimeType {
    /// Greatest/Least function returns non temporal time.
    GLRetNoneTemporal,
    /// Greatest/Least function returns date type, 'yyyy-mm-dd'.
    GLRetDate,
    /// Greatest/Least function returns datetime type, 'yyyy-mm-dd hh:mm:ss'.
    GLRetDatetime,
}

/// resolveType4Extremum gets compare type for GREATEST and LEAST and BETWEEN (mainly for
/// datetime).
// go: resolveType4Extremum L1
pub fn resolve_type4_extremum(
    ctx: &dyn EvalContext,
    args: &[ExprRef],
) -> (FieldType, GLRetTimeType, GLCmpStringMode) {
    let agg_type = aggregate_type(ctx, args);
    let mut cmp_string_mode = GLCmpStringMode::GLCmpStringDirectly;
    if agg_type.eval_type().is_string_kind() {
        let mut temporal_item: Option<FieldType> = None;
        for a in args {
            let item = a.get_type(ctx);
            // Find the temporal value in the arguments but prefer DateTime value.
            if tidb_types::etc::is_type_temporal(item.get_type())
                && (temporal_item.is_none() || item.get_type() == TypeDatetime)
            {
                temporal_item = Some(item.into_owned());
            }
        }

        if !tidb_types::etc::is_type_temporal(agg_type.get_type())
            && let Some(t) = &temporal_item
            && is_temporal_with_date(t.get_type())
        {
            cmp_string_mode = if t.get_type() == mysql::TypeDate {
                GLCmpStringMode::GLCmpStringAsDate
            } else {
                GLCmpStringMode::GLCmpStringAsDatetime
            };
        }
        // TODO: String charset, collation checking are needed.
    }
    let time_type = if agg_type.get_type() == mysql::TypeDate {
        GLRetTimeType::GLRetDate
    } else if agg_type.get_type() == TypeDatetime || agg_type.get_type() == TypeTimestamp {
        GLRetTimeType::GLRetDatetime
    } else {
        GLRetTimeType::GLRetNoneTemporal
    };
    (agg_type, time_type, cmp_string_mode)
}

/// unsupportedJSONComparison reports warnings while there is a JSON type in
/// least/greatest function's arguments.
// go: unsupportedJSONComparison L1
fn unsupported_json_comparison(ctx: &dyn BuildContext, args: &[ExprRef]) {
    let ectx = ctx.get_eval_ctx();
    for a in args {
        if a.get_type(&*ectx).get_type() == TypeJSON {
            ectx.append_warning(crate::errors::errUnsupportedJSONComparison.clone());
            break;
        }
    }
}

/// GREATEST or LEAST: one class over the two (`greatestFunctionClass`, `leastFunctionClass`).
#[derive(Debug)]
pub struct ExtremumFunctionClass {
    /// The embedded baseFunctionClass.
    pub base: BaseFunctionClass,
    greatest: bool,
}

/// The function class of `greatest`.
pub static GREATEST_CLASS: ExtremumFunctionClass = ExtremumFunctionClass {
    base: BaseFunctionClass {
        func_name: ast::Greatest,
        min_args: 2,
        max_args: -1,
    },
    greatest: true,
};

/// The function class of `least`.
pub static LEAST_CLASS: ExtremumFunctionClass = ExtremumFunctionClass {
    base: BaseFunctionClass {
        func_name: ast::Least,
        min_args: 2,
        max_args: -1,
    },
    greatest: false,
};

impl FunctionClass for ExtremumFunctionClass {
    // go: greatestFunctionClass.getFunction L1
    // go: leastFunctionClass.getFunction L1
    fn get_function(
        &self,
        ctx: &dyn BuildContext,
        args: Vec<ExprRef>,
    ) -> Result<BuiltinFunc, Error> {
        self.base.verify_args(&args)?;
        let (res_field_type, field_time_type, cmp_string_mode) =
            resolve_type4_extremum(&*ctx.get_eval_ctx(), &args);
        let mut res_tp = res_field_type.eval_type();
        let mut arg_tp = res_tp;
        if cmp_string_mode != GLCmpStringMode::GLCmpStringDirectly {
            // Args are temporal and string mixed, we cast all args as string and parse it to
            // temporal manually to compare.
            arg_tp = ETString;
        } else if res_tp == ETJson {
            unsupported_json_comparison(ctx, &args);
            arg_tp = ETString;
            res_tp = ETString;
        }
        let arg_tps = vec![arg_tp; args.len()];
        let mut bf =
            new_base_builtin_func_with_tp(ctx, self.base.func_name, args, res_tp, &arg_tps)?;
        let g = self.greatest;
        let sig: &'static SigDesc = match arg_tp {
            ETInt => {
                bf.tp.add_flag(res_field_type.get_flag());
                if g {
                    &builtinGreatestIntSig
                } else {
                    &builtinLeastIntSig
                }
            }
            ETReal => {
                if g {
                    &builtinGreatestRealSig
                } else {
                    &builtinLeastRealSig
                }
            }
            ETDecimal => {
                if g {
                    &builtinGreatestDecimalSig
                } else {
                    &builtinLeastDecimalSig
                }
            }
            ETString => match cmp_string_mode {
                GLCmpStringMode::GLCmpStringAsDate => {
                    bf.extra = SigExtra::Extremum { cmp_as_date: true };
                    if g {
                        &builtinGreatestCmpStringAsDateSig
                    } else {
                        &builtinLeastCmpStringAsDateSig
                    }
                }
                GLCmpStringMode::GLCmpStringAsDatetime => {
                    bf.extra = SigExtra::Extremum { cmp_as_date: false };
                    if g {
                        &builtinGreatestCmpStringAsTimeSig
                    } else {
                        &builtinLeastCmpStringAsTimeSig
                    }
                }
                GLCmpStringMode::GLCmpStringDirectly => {
                    if g {
                        &builtinGreatestStringSig
                    } else {
                        &builtinLeastStringSig
                    }
                }
            },
            ETDuration => {
                if g {
                    &builtinGreatestDurationSig
                } else {
                    &builtinLeastDurationSig
                }
            }
            ETDatetime | ETTimestamp => {
                let as_date = field_time_type == GLRetTimeType::GLRetDate;
                bf.extra = SigExtra::Extremum {
                    cmp_as_date: as_date,
                };
                match (g, as_date) {
                    (true, true) => &builtinGreatestDateSig,
                    (true, false) => &builtinGreatestTimeSig,
                    (false, true) => &builtinLeastDateSig,
                    (false, false) => &builtinLeastTimeSig,
                }
            }
            ETVectorFloat32 => {
                if g {
                    &builtinGreatestVectorFloat32Sig
                } else {
                    &builtinLeastVectorFloat32Sig
                }
            }
            other => {
                return Err(Error::plain(format!(
                    "unsupported type {} during evaluation",
                    other.to_string()
                )));
            }
        };
        // Go passes `args` to newBaseBuiltinFuncWithTp, which wraps the casts into that
        // same slice: the flen / decimal are those of the wrapped arguments.
        let (flen, decimal) = {
            let ectx = ctx.get_eval_ctx();
            fix_flen_and_decimal_for_greatest_and_least(&*ectx, &bf.args)
        };
        let mut f = bf.with_sig(sig);
        // Go: sig.getRetTp().SetFlenUnderLimit(..) — getRetTp runs on the type of before.
        f.get_ret_tp();
        f.tp.set_flen_under_limit(flen);
        f.get_ret_tp();
        f.tp.set_decimal_under_limit(decimal);
        Ok(f)
    }
}

/// The largest flen and decimal of the arguments.
// go: fixFlenAndDecimalForGreatestAndLeast L1
fn fix_flen_and_decimal_for_greatest_and_least(
    ctx: &dyn EvalContext,
    args: &[ExprRef],
) -> (isize, isize) {
    let (mut flen, mut decimal) = (0, 0);
    for a in args {
        let t = a.get_type(ctx);
        flen = flen.max(t.get_flen());
        decimal = decimal.max(t.get_decimal());
    }
    (flen, decimal)
}

// The bodies are Go's row versions (`evalXxx`): they stop at the first NULL argument (and
// at the first error), so later arguments are not evaluated. Go's vector versions evaluate
// every argument column and merge (`MergeNulls`), which is what the lazy body's vector
// driver does over the evaluated columns (spec §1c).

/// `builtinGreatestIntSig`.
// go: builtinGreatestIntSig.evalInt L1
// go: pkg/expression/builtin_compare_vec.go builtinGreatestIntSig.vecEvalInt L2 (the lazy body's vector driver over the evaluated columns)
// go: builtinGreatestIntSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: pkg/expression/builtin_compare_vec.go builtinGreatestIntSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Int, lazy, args(Int))]
pub fn greatest_int(
    ctx: &dyn EvalContext,
    _f: &impl SigView,
    args: &mut impl Args,
) -> Result<Option<i64>, Error> {
    let Some(mut res) = args.int(ctx, 0)? else {
        return Ok(None);
    };
    for i in 1..args.len() {
        let Some(v) = args.int(ctx, i)? else {
            return Ok(None);
        };
        if v > res {
            res = v;
        }
    }
    Ok(Some(res))
}

/// `builtinLeastIntSig`.
// go: builtinLeastIntSig.evalInt L1
// go: pkg/expression/builtin_compare_vec.go builtinLeastIntSig.vecEvalInt L2 (the lazy body's vector driver over the evaluated columns)
// go: builtinLeastIntSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: pkg/expression/builtin_compare_vec.go builtinLeastIntSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Int, lazy, args(Int))]
pub fn least_int(
    ctx: &dyn EvalContext,
    _f: &impl SigView,
    args: &mut impl Args,
) -> Result<Option<i64>, Error> {
    let Some(mut res) = args.int(ctx, 0)? else {
        return Ok(None);
    };
    for i in 1..args.len() {
        let Some(v) = args.int(ctx, i)? else {
            return Ok(None);
        };
        if v < res {
            res = v;
        }
    }
    Ok(Some(res))
}

/// `builtinGreatestRealSig`.
// go: builtinGreatestRealSig.evalReal L1
// go: pkg/expression/builtin_compare_vec.go builtinGreatestRealSig.vecEvalReal L2 (the lazy body's vector driver over the evaluated columns)
// go: builtinGreatestRealSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: pkg/expression/builtin_compare_vec.go builtinGreatestRealSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Real, lazy, args(Real))]
pub fn greatest_real(
    ctx: &dyn EvalContext,
    _f: &impl SigView,
    args: &mut impl Args,
) -> Result<Option<f64>, Error> {
    let Some(mut res) = args.real(ctx, 0)? else {
        return Ok(None);
    };
    for i in 1..args.len() {
        let Some(v) = args.real(ctx, i)? else {
            return Ok(None);
        };
        if v > res {
            res = v;
        }
    }
    Ok(Some(res))
}

/// `builtinLeastRealSig`.
// go: builtinLeastRealSig.evalReal L1
// go: pkg/expression/builtin_compare_vec.go builtinLeastRealSig.vecEvalReal L2 (the lazy body's vector driver over the evaluated columns)
// go: builtinLeastRealSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: pkg/expression/builtin_compare_vec.go builtinLeastRealSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Real, lazy, args(Real))]
pub fn least_real(
    ctx: &dyn EvalContext,
    _f: &impl SigView,
    args: &mut impl Args,
) -> Result<Option<f64>, Error> {
    let Some(mut res) = args.real(ctx, 0)? else {
        return Ok(None);
    };
    for i in 1..args.len() {
        let Some(v) = args.real(ctx, i)? else {
            return Ok(None);
        };
        if v < res {
            res = v;
        }
    }
    Ok(Some(res))
}

/// `builtinGreatestDecimalSig`.
// go: builtinGreatestDecimalSig.evalDecimal L1
// go: pkg/expression/builtin_compare_vec.go builtinGreatestDecimalSig.vecEvalDecimal L2 (the lazy body's vector driver over the evaluated columns)
// go: builtinGreatestDecimalSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: pkg/expression/builtin_compare_vec.go builtinGreatestDecimalSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Decimal, lazy, args(Decimal))]
pub fn greatest_decimal(
    ctx: &dyn EvalContext,
    _f: &impl SigView,
    args: &mut impl Args,
) -> Result<Option<MyDecimal>, Error> {
    let Some(mut res) = args.decimal(ctx, 0)? else {
        return Ok(None);
    };
    for i in 1..args.len() {
        let Some(v) = args.decimal(ctx, i)? else {
            return Ok(None);
        };
        if v.compare(&res) > 0 {
            res = v;
        }
    }
    Ok(Some(res))
}

/// `builtinLeastDecimalSig`.
// go: builtinLeastDecimalSig.evalDecimal L1
// go: pkg/expression/builtin_compare_vec.go builtinLeastDecimalSig.vecEvalDecimal L2 (the lazy body's vector driver over the evaluated columns)
// go: builtinLeastDecimalSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: pkg/expression/builtin_compare_vec.go builtinLeastDecimalSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Decimal, lazy, args(Decimal))]
pub fn least_decimal(
    ctx: &dyn EvalContext,
    _f: &impl SigView,
    args: &mut impl Args,
) -> Result<Option<MyDecimal>, Error> {
    let Some(mut res) = args.decimal(ctx, 0)? else {
        return Ok(None);
    };
    for i in 1..args.len() {
        let Some(v) = args.decimal(ctx, i)? else {
            return Ok(None);
        };
        if v.compare(&res) < 0 {
            res = v;
        }
    }
    Ok(Some(res))
}

/// `builtinGreatestStringSig`: the row version keeps the earlier argument on a tie under the
/// collation (`CompareString(v, maxv) > 0`); the vector version keeps the later one
/// (`CompareString(src, arg) > 0` keeps `src`, else `arg`). Each driver follows its Go
/// version (D08.1 spec §3.4 (c); D08.4 spec §1c).
// go: builtinGreatestStringSig.evalString L1
// go: pkg/expression/builtin_compare_vec.go builtinGreatestStringSig.vecEvalString L2 (the lazy body's vector driver; SigView::vector picks the vector version's tie rule)
// go: builtinGreatestStringSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: pkg/expression/builtin_compare_vec.go builtinGreatestStringSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(String, lazy, args(String))]
pub fn greatest_string(
    ctx: &dyn EvalContext,
    f: &impl SigView,
    args: &mut impl Args,
    out: &mut Vec<u8>,
) -> Result<Option<()>, Error> {
    extremum_string(ctx, f, args, out, 1)
}

/// `builtinLeastStringSig` (tie rule as in [`greatest_string`]).
// go: builtinLeastStringSig.evalString L1
// go: pkg/expression/builtin_compare_vec.go builtinLeastStringSig.vecEvalString L2 (the lazy body's vector driver; SigView::vector picks the vector version's tie rule)
// go: builtinLeastStringSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: pkg/expression/builtin_compare_vec.go builtinLeastStringSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(String, lazy, args(String))]
pub fn least_string(
    ctx: &dyn EvalContext,
    f: &impl SigView,
    args: &mut impl Args,
    out: &mut Vec<u8>,
) -> Result<Option<()>, Error> {
    extremum_string(ctx, f, args, out, -1)
}

/// The string GREATEST (`sign` 1) / LEAST (`sign` -1) of the row and vector versions.
// glue: shared body of builtinGreatestStringSig / builtinLeastStringSig
fn extremum_string(
    ctx: &dyn EvalContext,
    f: &impl SigView,
    args: &mut impl Args,
    out: &mut Vec<u8>,
    sign: isize,
) -> Result<Option<()>, Error> {
    let collation = f.collation_info.collation.clone();
    let Some(first) = args.string(ctx, 0)? else {
        return Ok(None);
    };
    let mut res = first.into_owned();
    for i in 1..args.len() {
        let Some(v) = args.string(ctx, i)? else {
            return Ok(None);
        };
        let take = if f.vector() {
            // dst = CompareString(src, arg) {>,<} 0 ? src : arg
            tidb_types::compare::compare_string(&res, &v, &collation) * sign <= 0
        } else {
            tidb_types::compare::compare_string(&v, &res, &collation) * sign > 0
        };
        if take {
            res = v.into_owned();
        }
    }
    out.extend_from_slice(&res);
    Ok(Some(()))
}

/// doTimeConversionForGL: the string as a DATE (`cmp_as_date`) or DATETIME string; an
/// invalid value is handled by `handleInvalidTimeError` and kept as it is.
// go: doTimeConversionForGL L1
fn do_time_conversion_for_gl(
    cmp_as_date: bool,
    ctx: &dyn EvalContext,
    str_val: &[u8],
) -> Result<Vec<u8>, Error> {
    let tc = type_ctx(ctx);
    let (t, r) = if cmp_as_date {
        let (t, r) = tidb_types::time::parse_date(&tc, str_val);
        match r {
            Ok(()) => t.convert(&tc, mysql::TypeDate),
            Err(e) => (t, Err(e)),
        }
    } else {
        let (t, r) = tidb_types::time::parse_datetime_str(&tc, str_val);
        match r {
            Ok(()) => t.convert(&tc, TypeDatetime),
            Err(e) => (t, Err(e)),
        }
    };
    match r {
        Err(e) => {
            crate::errors::handle_invalid_time_error(ctx, e)?;
            Ok(str_val.to_vec())
        }
        Ok(()) => Ok(t.string()),
    }
}

/// The row version of the GREATEST (`sign` 1) / LEAST (`sign` -1) of strings compared as
/// times (`builtinGreatest/LeastCmpStringAsTimeSig.evalString`).
// glue: shared body of builtinGreatestCmpStringAsTimeSig / builtinLeastCmpStringAsTimeSig
fn extremum_cmp_string_as_time(
    ctx: &dyn EvalContext,
    f: &impl SigView,
    args: &mut impl Args,
    out: &mut Vec<u8>,
    sign: i32,
) -> Result<Option<()>, Error> {
    let mut res: Vec<u8> = Vec::new();
    for i in 0..args.len() {
        let Some(v) = args.string(ctx, i)? else {
            return Ok(None);
        };
        let v = do_time_conversion_for_gl(f.cmp_as_date(), ctx, &v)?;
        // In MySQL, if the compare result is zero, than we will try to use the string
        // comparison result.
        if i == 0 || (v.as_slice().cmp(res.as_slice()) as i32) * sign > 0 {
            res = v;
        }
    }
    out.extend_from_slice(&res);
    Ok(Some(()))
}

/// The vector version of the strings compared as times (`ArgsFallback::OwnArgs`): argument
/// `j` is evaluated as a column, then every row that is not NULL yet is converted and
/// compared, before argument `j + 1` (Go `builtinGreatest/LeastCmpStringAsTimeSig
/// .vecEvalString`): the warnings and the first error come in that order.
// glue: shared vector version of builtinGreatestCmpStringAsTimeSig / builtinLeastCmpStringAsTimeSig
fn extremum_cmp_string_as_time_vec(
    f: &BuiltinFunc,
    ctx: &dyn EvalContext,
    eval_arg: &mut crate::eval::ArgColumnFn<'_>,
    input: &tidb_chunk::Chunk,
    result: &mut tidb_chunk::Column,
    sign: i32,
) -> Result<(), Error> {
    let n = input.num_rows();
    let mut dst_strings: Vec<Vec<u8>> = vec![Vec::new(); n];
    let mut dst_null_map = vec![false; n];
    for j in 0..f.args.len() {
        eval_arg(j, result)?;
        for i in 0..n {
            dst_null_map[i] = dst_null_map[i] || result.is_null(i);
            if dst_null_map[i] {
                continue;
            }
            let arg_time_str =
                do_time_conversion_for_gl(f.cmp_as_date(), ctx, result.get_bytes(i))?;
            if j == 0 || (arg_time_str.as_slice().cmp(dst_strings[i].as_slice()) as i32) * sign > 0
            {
                dst_strings[i] = arg_time_str;
            }
        }
    }
    crate::eval::rt::reset_string(result, n);
    for i in 0..n {
        if dst_null_map[i] {
            result.append_null();
        } else {
            result.append_bytes(&dst_strings[i]);
        }
    }
    Ok(())
}

/// `builtinGreatestCmpStringAsTimeSig` (`cmpAsDate` in `SigExtra::Extremum`).
// go: builtinGreatestCmpStringAsTimeSig.evalString L1
// go: builtinGreatestCmpStringAsTimeSig.Clone L2 (BuiltinFunc::clone_from_base copies SigExtra::Extremum, as Go)
// go: pkg/expression/builtin_compare_vec.go builtinGreatestCmpStringAsTimeSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(String, lazy, args(String))]
pub fn greatest_cmp_string_as_time(
    ctx: &dyn EvalContext,
    f: &impl SigView,
    args: &mut impl Args,
    out: &mut Vec<u8>,
) -> Result<Option<()>, Error> {
    extremum_cmp_string_as_time(ctx, f, args, out, 1)
}

/// `builtinGreatestCmpStringAsTimeSig.vecEvalString`.
// go: pkg/expression/builtin_compare_vec.go builtinGreatestCmpStringAsTimeSig.vecEvalString L1
fn greatest_cmp_string_as_time_vec(
    f: &BuiltinFunc,
    ctx: &dyn EvalContext,
    _st: &EvalState,
    eval_arg: &mut crate::eval::ArgColumnFn<'_>,
    input: &tidb_chunk::Chunk,
    result: &mut tidb_chunk::Column,
) -> Result<(), Error> {
    extremum_cmp_string_as_time_vec(f, ctx, eval_arg, input, result, 1)
}

/// `builtinLeastCmpStringAsTimeSig` (`cmpAsDate` in `SigExtra::Extremum`).
// go: builtinLeastCmpStringAsTimeSig.evalString L1
// go: builtinLeastCmpStringAsTimeSig.Clone L2 (BuiltinFunc::clone_from_base copies SigExtra::Extremum, as Go)
// go: pkg/expression/builtin_compare_vec.go builtinLeastCmpStringAsTimeSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(String, lazy, args(String))]
pub fn least_cmp_string_as_time(
    ctx: &dyn EvalContext,
    f: &impl SigView,
    args: &mut impl Args,
    out: &mut Vec<u8>,
) -> Result<Option<()>, Error> {
    extremum_cmp_string_as_time(ctx, f, args, out, -1)
}

/// `builtinLeastCmpStringAsTimeSig.vecEvalString`.
// go: pkg/expression/builtin_compare_vec.go builtinLeastCmpStringAsTimeSig.vecEvalString L1
fn least_cmp_string_as_time_vec(
    f: &BuiltinFunc,
    ctx: &dyn EvalContext,
    _st: &EvalState,
    eval_arg: &mut crate::eval::ArgColumnFn<'_>,
    input: &tidb_chunk::Chunk,
    result: &mut tidb_chunk::Column,
) -> Result<(), Error> {
    extremum_cmp_string_as_time_vec(f, ctx, eval_arg, input, result, -1)
}

/// getAccurateTimeTypeForGLRet: DATE or DATETIME.
// go: getAccurateTimeTypeForGLRet L1
fn get_accurate_time_type_for_gl_ret(cmp_as_date: bool) -> u8 {
    if cmp_as_date {
        mysql::TypeDate
    } else {
        TypeDatetime
    }
}

/// The GREATEST (`sign` 1) / LEAST (`sign` -1) of times, converted to DATE or DATETIME. The
/// row version handles a conversion error with `handleInvalidTimeError` (NULL after a
/// warning); the vector version returns it (Go `vecEvalTime`).
// glue: shared body of builtinGreatestTimeSig / builtinLeastTimeSig
fn extremum_time(
    ctx: &dyn EvalContext,
    f: &impl SigView,
    args: &mut impl Args,
    sign: isize,
) -> Result<Option<Time>, Error> {
    let mut res = Time::default();
    for i in 0..args.len() {
        let Some(v) = args.time(ctx, i)? else {
            return Ok(None);
        };
        if i == 0 || v.compare(&res) * sign > 0 {
            res = v;
        }
    }
    // Convert ETType Time value to MySQL actual type, distinguish date and datetime.
    let tc = type_ctx(ctx);
    let res_time_tp = get_accurate_time_type_for_gl_ret(f.cmp_as_date());
    let (res, r) = res.convert(&tc, res_time_tp);
    if let Err(e) = r {
        if f.vector() {
            return Err(e);
        }
        crate::errors::handle_invalid_time_error(ctx, e)?;
        return Ok(None);
    }
    Ok(Some(res))
}

/// `builtinGreatestTimeSig` (`cmpAsDate` in `SigExtra::Extremum`).
// go: builtinGreatestTimeSig.evalTime L1
// go: pkg/expression/builtin_compare_vec.go builtinGreatestTimeSig.vecEvalTime L2 (the lazy body's vector driver; SigView::vector: a conversion error is returned, not handled)
// go: builtinGreatestTimeSig.Clone L2 (BuiltinFunc::clone_from_base copies SigExtra::Extremum, as Go)
// go: pkg/expression/builtin_compare_vec.go builtinGreatestTimeSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Time, lazy, args(Time))]
pub fn greatest_time(
    ctx: &dyn EvalContext,
    f: &impl SigView,
    args: &mut impl Args,
) -> Result<Option<Time>, Error> {
    extremum_time(ctx, f, args, 1)
}

/// `builtinLeastTimeSig` (`cmpAsDate` in `SigExtra::Extremum`).
// go: builtinLeastTimeSig.evalTime L1
// go: pkg/expression/builtin_compare_vec.go builtinLeastTimeSig.vecEvalTime L2 (the lazy body's vector driver; SigView::vector: a conversion error is returned, not handled)
// go: builtinLeastTimeSig.Clone L2 (BuiltinFunc::clone_from_base copies SigExtra::Extremum, as Go)
// go: pkg/expression/builtin_compare_vec.go builtinLeastTimeSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Time, lazy, args(Time))]
pub fn least_time(
    ctx: &dyn EvalContext,
    f: &impl SigView,
    args: &mut impl Args,
) -> Result<Option<Time>, Error> {
    extremum_time(ctx, f, args, -1)
}

/// `builtinGreatestDurationSig`.
// go: builtinGreatestDurationSig.evalDuration L1
// go: pkg/expression/builtin_compare_vec.go builtinGreatestDurationSig.vecEvalDuration L2 (the lazy body's vector driver over the evaluated columns)
// go: builtinGreatestDurationSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: pkg/expression/builtin_compare_vec.go builtinGreatestDurationSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Duration, lazy, args(Duration))]
pub fn greatest_duration(
    ctx: &dyn EvalContext,
    _f: &impl SigView,
    args: &mut impl Args,
) -> Result<Option<Duration>, Error> {
    let mut res = Duration::default();
    for i in 0..args.len() {
        let Some(v) = args.duration(ctx, i)? else {
            return Ok(None);
        };
        if i == 0 || v.compare(res) > 0 {
            res = v;
        }
    }
    Ok(Some(res))
}

/// `builtinLeastDurationSig`.
// go: builtinLeastDurationSig.evalDuration L1
// go: pkg/expression/builtin_compare_vec.go builtinLeastDurationSig.vecEvalDuration L2 (the lazy body's vector driver over the evaluated columns)
// go: builtinLeastDurationSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: pkg/expression/builtin_compare_vec.go builtinLeastDurationSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Duration, lazy, args(Duration))]
pub fn least_duration(
    ctx: &dyn EvalContext,
    _f: &impl SigView,
    args: &mut impl Args,
) -> Result<Option<Duration>, Error> {
    let mut res = Duration::default();
    for i in 0..args.len() {
        let Some(v) = args.duration(ctx, i)? else {
            return Ok(None);
        };
        if i == 0 || v.compare(res) < 0 {
            res = v;
        }
    }
    Ok(Some(res))
}

/// `builtinGreatestVectorFloat32Sig` (no vector version in Go).
// go: builtinGreatestVectorFloat32Sig.evalVectorFloat32 L1
// go: builtinGreatestVectorFloat32Sig.Clone L2 (BuiltinFunc::clone_from_base)
#[eval_fn(VectorFloat32, lazy, args(VectorFloat32), novec)]
pub fn greatest_vector_float32(
    ctx: &dyn EvalContext,
    _f: &impl SigView,
    args: &mut impl Args,
) -> Result<Option<VectorFloat32<'static>>, Error> {
    let mut res: Option<VectorFloat32<'static>> = None;
    for i in 0..args.len() {
        let Some(v) = args.vector_float32(ctx, i)? else {
            return Ok(None);
        };
        if i == 0 || res.as_ref().is_some_and(|r| v.compare(r) > 0) {
            res = Some(v.clone());
        }
    }
    Ok(res)
}

/// `builtinLeastVectorFloat32Sig` (no vector version in Go).
// go: builtinLeastVectorFloat32Sig.evalVectorFloat32 L1
// go: builtinLeastVectorFloat32Sig.Clone L2 (BuiltinFunc::clone_from_base)
#[eval_fn(VectorFloat32, lazy, args(VectorFloat32), novec)]
pub fn least_vector_float32(
    ctx: &dyn EvalContext,
    _f: &impl SigView,
    args: &mut impl Args,
) -> Result<Option<VectorFloat32<'static>>, Error> {
    let mut res: Option<VectorFloat32<'static>> = None;
    for i in 0..args.len() {
        let Some(v) = args.vector_float32(ctx, i)? else {
            return Ok(None);
        };
        if i == 0 || res.as_ref().is_some_and(|r| v.compare(r) < 0) {
            res = Some(v.clone());
        }
    }
    Ok(res)
}

/// INTERVAL(N, N1, N2, ...): the index of the last argument that is less than or equal
/// to N.
#[derive(Debug)]
pub struct IntervalFunctionClass {
    /// The embedded baseFunctionClass.
    pub base: BaseFunctionClass,
}

/// The function class of `interval`.
pub static INTERVAL_CLASS: IntervalFunctionClass = IntervalFunctionClass {
    base: BaseFunctionClass {
        func_name: ast::Interval,
        min_args: 2,
        max_args: -1,
    },
};

impl FunctionClass for IntervalFunctionClass {
    // go: intervalFunctionClass.getFunction L1
    fn get_function(
        &self,
        ctx: &dyn BuildContext,
        args: Vec<ExprRef>,
    ) -> Result<BuiltinFunc, Error> {
        self.base.verify_args(&args)?;

        let mut all_int = true;
        let mut has_nullable = false;
        // if we have nullable columns in the argument list, we won't do a binary search,
        // instead we will linearly scan the arguments (as MySQL does).
        {
            let ectx = ctx.get_eval_ctx();
            for a in &args {
                let tp = a.get_type(&*ectx);
                if tp.eval_type() != ETInt {
                    all_int = false;
                }
                if !has_not_null_flag(tp.get_flag()) {
                    has_nullable = true;
                }
            }
        }

        let arg_tp = if all_int { ETInt } else { ETReal };
        let arg_tps = vec![arg_tp; args.len()];
        let mut bf =
            new_base_builtin_func_with_tp(ctx, self.base.func_name, args, ETInt, &arg_tps)?;
        let sig: &'static SigDesc = if all_int {
            bf.extra = SigExtra::IntervalInt { has_nullable };
            &builtinIntervalIntSig
        } else {
            bf.extra = SigExtra::IntervalReal { has_nullable };
            &builtinIntervalRealSig
        };
        Ok(bf.with_sig(sig))
    }
}

/// `builtinIntervalIntSig.evalInt`: -1 for a NULL `N`; otherwise the linear (nullable
/// arguments) or binary search over `args[1:]`.
// go: builtinIntervalIntSig.evalInt L1
// go: builtinIntervalIntSig.Clone L2 (BuiltinFunc::clone_from_base: hasNullable is not copied, as Go)
// go: pkg/expression/builtin_compare_vec.go builtinIntervalIntSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Int, lazy, args(Int))]
pub fn interval_int(
    ctx: &dyn EvalContext,
    f: &impl SigView,
    args: &mut impl Args,
) -> Result<Option<i64>, Error> {
    let Some(arg0) = args.int(ctx, 0)? else {
        return Ok(Some(-1));
    };
    let is_uint1 = has_unsigned_flag(f.arg_type(0).get_flag());
    let idx = if f.has_nullable() {
        interval_int_linear_search(ctx, f, arg0, is_uint1, args)?
    } else {
        interval_int_bin_search(ctx, f, arg0, is_uint1, args)?
    };
    Ok(Some(idx as i64))
}

/// Whether `target` is less than `arg` with the signedness of both.
// glue: the comparison of builtinIntervalIntSig.linearSearch / binSearch
fn interval_int_less(target: i64, is_uint1: bool, arg: i64, is_uint2: bool) -> bool {
    match (is_uint1, is_uint2) {
        (false, false) => target < arg,
        (true, true) => (target as u64) < (arg as u64),
        (false, true) => target < 0 || (target as u64) < (arg as u64),
        (true, false) => arg > 0 && (target as u64) < (arg as u64),
    }
}

/// linearSearch linearly scans the argument least to find the position of the first value
/// that is larger than the given target (the arguments are `args[1:]`).
// go: builtinIntervalIntSig.linearSearch L1
fn interval_int_linear_search(
    ctx: &dyn EvalContext,
    f: &impl SigView,
    target: i64,
    is_uint1: bool,
    args: &mut impl Args,
) -> Result<usize, Error> {
    let n = args.len() - 1;
    let mut i = 0;
    while i < n {
        let is_uint2 = has_unsigned_flag(f.arg_type(i + 1).get_flag());
        let arg = args.int(ctx, i + 1)?;
        let less = match arg {
            Some(arg) => interval_int_less(target, is_uint1, arg, is_uint2),
            None => false,
        };
        if less {
            break;
        }
        i += 1;
    }
    Ok(i)
}

/// binSearch is a binary search method. All arguments are treated as integers. It is
/// required that arg[0] < args[1] < args[2] < ... < args[n] for this function to work
/// correctly.
// go: builtinIntervalIntSig.binSearch L1
fn interval_int_bin_search(
    ctx: &dyn EvalContext,
    f: &impl SigView,
    target: i64,
    is_uint1: bool,
    args: &mut impl Args,
) -> Result<usize, Error> {
    let (mut i, mut j) = (0usize, args.len() - 1);
    while i < j {
        let mid = i + (j - i) / 2;
        let v = args.int(ctx, mid + 1)?.unwrap_or(target);
        let is_uint2 = has_unsigned_flag(f.arg_type(mid + 1).get_flag());
        if !interval_int_less(target, is_uint1, v, is_uint2) {
            i = mid + 1;
        } else {
            j = mid;
        }
    }
    Ok(i)
}

/// `builtinIntervalRealSig.evalInt`.
// go: builtinIntervalRealSig.evalInt L1
// go: builtinIntervalRealSig.Clone L2 (BuiltinFunc::clone_from_base copies SigExtra::IntervalReal, as Go)
// go: pkg/expression/builtin_compare_vec.go builtinIntervalRealSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Int, lazy, args(Real))]
pub fn interval_real(
    ctx: &dyn EvalContext,
    f: &impl SigView,
    args: &mut impl Args,
) -> Result<Option<i64>, Error> {
    let Some(arg0) = args.real(ctx, 0)? else {
        return Ok(Some(-1));
    };
    let idx = if f.has_nullable() {
        interval_real_linear_search(ctx, arg0, args)?
    } else {
        interval_real_bin_search(ctx, arg0, args)?
    };
    Ok(Some(idx as i64))
}

// go: builtinIntervalRealSig.linearSearch L1
fn interval_real_linear_search(
    ctx: &dyn EvalContext,
    target: f64,
    args: &mut impl Args,
) -> Result<usize, Error> {
    let n = args.len() - 1;
    let mut i = 0;
    while i < n {
        if let Some(arg) = args.real(ctx, i + 1)?
            && target < arg
        {
            break;
        }
        i += 1;
    }
    Ok(i)
}

// go: builtinIntervalRealSig.binSearch L1
fn interval_real_bin_search(
    ctx: &dyn EvalContext,
    target: f64,
    args: &mut impl Args,
) -> Result<usize, Error> {
    let (mut i, mut j) = (0usize, args.len() - 1);
    while i < j {
        let mid = i + (j - i) / 2;
        match args.real(ctx, mid + 1)? {
            None => i = mid + 1,
            // Go: `cmp := target < v; !cmp` (a NaN is not less: i = mid + 1)
            Some(v) if target < v => j = mid,
            Some(_) => i = mid + 1,
        }
    }
    Ok(i)
}

/// The vector versions of INTERVAL (`ArgsFallback::OwnArgs`): `args[0]` is evaluated as a
/// column, the search of each row evaluates `args[1:]` on the row. A NULL `N` is -1 (not
/// NULL).
// go: pkg/expression/builtin_compare_vec.go builtinIntervalIntSig.vecEvalInt L2 (args[0] is an RPN sub-program; the row search reads it from the column)
// go: pkg/expression/builtin_compare_vec.go builtinIntervalRealSig.vecEvalInt L2 (args[0] is an RPN sub-program; the row search reads it from the column)
fn interval_vec_eval_int(
    f: &BuiltinFunc,
    ctx: &dyn EvalContext,
    st: &EvalState,
    eval_arg: &mut crate::eval::ArgColumnFn<'_>,
    input: &tidb_chunk::Chunk,
    result: &mut tidb_chunk::Column,
) -> Result<(), Error> {
    let mut col0 = crate::eval::rt::empty_column();
    eval_arg(0, &mut col0)?;
    let n = input.num_rows();
    let view = crate::eval::RowSig::new(f, ctx);
    let mut out = crate::eval::rt::IntOut::new(result, n, &[]);
    for i in 0..n {
        let mut a = crate::eval::rt::FirstColArgs::new(f, st, input.get_row(i), &col0, i);
        let v = if std::ptr::eq(f.sig, &builtinIntervalIntSig) {
            interval_int(ctx, &view, &mut a)?
        } else {
            interval_real(ctx, &view, &mut a)?
        };
        match v {
            Some(v) => out.set(i, v),
            None => out.set_null(i),
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- signature descriptors

macro_rules! cmp_sig {
    ($name:ident, $pb:expr, $vec:expr, $drivers:ident) => {
        cmp_sig!($name, $pb, $vec, $drivers, None);
    };
    ($name:ident, $pb:expr, $vec:expr, $drivers:ident, $fallback:ident) => {
        #[doc = concat!("`", stringify!($name), "`.")]
        pub static $name: SigDesc = SigDesc {
            name: stringify!($name),
            pb_code: $pb,
            vectorized: $vec,
            drivers: $drivers,
            args_fallback: crate::eval::ArgsFallback::$fallback,
        };
    };
}

// COALESCE (go-tipb ScalarFuncSig_Coalesce*; the VECTOR signature has no code: Go comments
// its setPbCode out and the VECTOR signature has no vector version).
cmp_sig!(builtinCoalesceIntSig, 4201, true, COALESCE_INT, EveryArg);
cmp_sig!(builtinCoalesceRealSig, 4202, true, COALESCE_REAL, EveryArg);
cmp_sig!(
    builtinCoalesceDecimalSig,
    4203,
    true,
    COALESCE_DECIMAL,
    EveryArg
);
cmp_sig!(
    builtinCoalesceStringSig,
    4204,
    true,
    COALESCE_STRING,
    EveryArg
);
cmp_sig!(builtinCoalesceTimeSig, 4205, true, COALESCE_TIME, EveryArg);
cmp_sig!(
    builtinCoalesceDurationSig,
    4206,
    true,
    COALESCE_DURATION,
    EveryArg
);
cmp_sig!(builtinCoalesceJSONSig, 4207, true, COALESCE_JSON, EveryArg);
cmp_sig!(
    builtinCoalesceVectorFloat32Sig,
    0,
    false,
    COALESCE_VECTOR_FLOAT32
);

// GREATEST / LEAST / INTERVAL (go-tipb ScalarFuncSig_*; the VECTOR signatures have no code:
// Go comments their setPbCode out). The time signatures with cmpAsDate have their own code
// (GreatestDate, GreatestCmpStringAsDate, ...) under the same Go type.
macro_rules! gl_sig {
    ($static:ident, $go:literal, $pb:expr, $vec:expr, $drivers:expr) => {
        gl_sig!($static, $go, $pb, $vec, $drivers, None);
    };
    ($static:ident, $go:literal, $pb:expr, $vec:expr, $drivers:expr, $fallback:expr) => {
        #[doc = concat!("`", $go, "`.")]
        pub static $static: SigDesc = SigDesc {
            name: $go,
            pb_code: $pb,
            vectorized: $vec,
            drivers: $drivers,
            args_fallback: $fallback,
        };
    };
}
use crate::eval::ArgsFallback::None as NoFallback;
gl_sig!(
    builtinGreatestIntSig,
    "builtinGreatestIntSig",
    4215,
    true,
    GREATEST_INT,
    NoFallback
);
gl_sig!(
    builtinGreatestRealSig,
    "builtinGreatestRealSig",
    4216,
    true,
    GREATEST_REAL,
    NoFallback
);
gl_sig!(
    builtinGreatestDecimalSig,
    "builtinGreatestDecimalSig",
    4217,
    true,
    GREATEST_DECIMAL,
    NoFallback
);
gl_sig!(
    builtinGreatestStringSig,
    "builtinGreatestStringSig",
    4218,
    true,
    GREATEST_STRING,
    NoFallback
);
gl_sig!(
    builtinGreatestTimeSig,
    "builtinGreatestTimeSig",
    4219,
    true,
    GREATEST_TIME,
    NoFallback
);
gl_sig!(
    builtinGreatestDateSig,
    "builtinGreatestTimeSig",
    7075,
    true,
    GREATEST_TIME,
    NoFallback
);
gl_sig!(
    builtinGreatestDurationSig,
    "builtinGreatestDurationSig",
    7067,
    true,
    GREATEST_DURATION,
    NoFallback
);
gl_sig!(
    builtinGreatestCmpStringAsTimeSig,
    "builtinGreatestCmpStringAsTimeSig",
    7068,
    true,
    GREATEST_CMP_STRING_AS_TIME,
    crate::eval::ArgsFallback::OwnArgs(greatest_cmp_string_as_time_vec)
);
gl_sig!(
    builtinGreatestCmpStringAsDateSig,
    "builtinGreatestCmpStringAsTimeSig",
    7073,
    true,
    GREATEST_CMP_STRING_AS_TIME,
    crate::eval::ArgsFallback::OwnArgs(greatest_cmp_string_as_time_vec)
);
gl_sig!(
    builtinGreatestVectorFloat32Sig,
    "builtinGreatestVectorFloat32Sig",
    0,
    false,
    GREATEST_VECTOR_FLOAT32,
    NoFallback
);
gl_sig!(
    builtinLeastIntSig,
    "builtinLeastIntSig",
    4220,
    true,
    LEAST_INT,
    NoFallback
);
gl_sig!(
    builtinLeastRealSig,
    "builtinLeastRealSig",
    4221,
    true,
    LEAST_REAL,
    NoFallback
);
gl_sig!(
    builtinLeastDecimalSig,
    "builtinLeastDecimalSig",
    4222,
    true,
    LEAST_DECIMAL,
    NoFallback
);
gl_sig!(
    builtinLeastStringSig,
    "builtinLeastStringSig",
    4223,
    true,
    LEAST_STRING,
    NoFallback
);
gl_sig!(
    builtinLeastTimeSig,
    "builtinLeastTimeSig",
    4224,
    true,
    LEAST_TIME,
    NoFallback
);
gl_sig!(
    builtinLeastDateSig,
    "builtinLeastTimeSig",
    7076,
    true,
    LEAST_TIME,
    NoFallback
);
gl_sig!(
    builtinLeastDurationSig,
    "builtinLeastDurationSig",
    7069,
    true,
    LEAST_DURATION,
    NoFallback
);
gl_sig!(
    builtinLeastCmpStringAsTimeSig,
    "builtinLeastCmpStringAsTimeSig",
    7070,
    true,
    LEAST_CMP_STRING_AS_TIME,
    crate::eval::ArgsFallback::OwnArgs(least_cmp_string_as_time_vec)
);
gl_sig!(
    builtinLeastCmpStringAsDateSig,
    "builtinLeastCmpStringAsTimeSig",
    7074,
    true,
    LEAST_CMP_STRING_AS_TIME,
    crate::eval::ArgsFallback::OwnArgs(least_cmp_string_as_time_vec)
);
gl_sig!(
    builtinLeastVectorFloat32Sig,
    "builtinLeastVectorFloat32Sig",
    0,
    false,
    LEAST_VECTOR_FLOAT32,
    NoFallback
);
gl_sig!(
    builtinIntervalIntSig,
    "builtinIntervalIntSig",
    4225,
    true,
    INTERVAL_INT,
    crate::eval::ArgsFallback::OwnArgs(interval_vec_eval_int)
);
gl_sig!(
    builtinIntervalRealSig,
    "builtinIntervalRealSig",
    4226,
    true,
    INTERVAL_REAL,
    crate::eval::ArgsFallback::OwnArgs(interval_vec_eval_int)
);

// tipb.ScalarFuncSig codes (go-tipb expression.pb.go).
cmp_sig!(builtinLTIntSig, 100, true, LT_INT);
cmp_sig!(builtinLTRealSig, 101, true, LT_REAL);
cmp_sig!(builtinLTDecimalSig, 102, true, LT_DECIMAL);
cmp_sig!(builtinLTStringSig, 103, true, LT_STRING);
cmp_sig!(builtinLTTimeSig, 104, true, LT_TIME);
cmp_sig!(builtinLTDurationSig, 105, true, LT_DURATION);
cmp_sig!(builtinLTJSONSig, 106, true, LT_JSON);
cmp_sig!(builtinLTVectorFloat32Sig, 5146, false, LT_VECTOR_FLOAT32);
cmp_sig!(builtinLEIntSig, 110, true, LE_INT);
cmp_sig!(builtinLERealSig, 111, true, LE_REAL);
cmp_sig!(builtinLEDecimalSig, 112, true, LE_DECIMAL);
cmp_sig!(builtinLEStringSig, 113, true, LE_STRING);
cmp_sig!(builtinLETimeSig, 114, true, LE_TIME);
cmp_sig!(builtinLEDurationSig, 115, true, LE_DURATION);
cmp_sig!(builtinLEJSONSig, 116, true, LE_JSON);
cmp_sig!(builtinLEVectorFloat32Sig, 5147, false, LE_VECTOR_FLOAT32);
cmp_sig!(builtinGTIntSig, 120, true, GT_INT);
cmp_sig!(builtinGTRealSig, 121, true, GT_REAL);
cmp_sig!(builtinGTDecimalSig, 122, true, GT_DECIMAL);
cmp_sig!(builtinGTStringSig, 123, true, GT_STRING);
cmp_sig!(builtinGTTimeSig, 124, true, GT_TIME);
cmp_sig!(builtinGTDurationSig, 125, true, GT_DURATION);
cmp_sig!(builtinGTJSONSig, 126, true, GT_JSON);
cmp_sig!(builtinGTVectorFloat32Sig, 5148, false, GT_VECTOR_FLOAT32);
cmp_sig!(builtinGEIntSig, 130, true, GE_INT);
cmp_sig!(builtinGERealSig, 131, true, GE_REAL);
cmp_sig!(builtinGEDecimalSig, 132, true, GE_DECIMAL);
cmp_sig!(builtinGEStringSig, 133, true, GE_STRING);
cmp_sig!(builtinGETimeSig, 134, true, GE_TIME);
cmp_sig!(builtinGEDurationSig, 135, true, GE_DURATION);
cmp_sig!(builtinGEJSONSig, 136, true, GE_JSON);
cmp_sig!(builtinGEVectorFloat32Sig, 5151, false, GE_VECTOR_FLOAT32);
cmp_sig!(builtinEQIntSig, 140, true, EQ_INT);
cmp_sig!(builtinEQRealSig, 141, true, EQ_REAL);
cmp_sig!(builtinEQDecimalSig, 142, true, EQ_DECIMAL);
cmp_sig!(builtinEQStringSig, 143, true, EQ_STRING);
cmp_sig!(builtinEQTimeSig, 144, true, EQ_TIME);
cmp_sig!(builtinEQDurationSig, 145, true, EQ_DURATION);
cmp_sig!(builtinEQJSONSig, 146, true, EQ_JSON);
cmp_sig!(builtinEQVectorFloat32Sig, 5152, false, EQ_VECTOR_FLOAT32);
cmp_sig!(builtinNEIntSig, 150, true, NE_INT);
cmp_sig!(builtinNERealSig, 151, true, NE_REAL);
cmp_sig!(builtinNEDecimalSig, 152, true, NE_DECIMAL);
cmp_sig!(builtinNEStringSig, 153, true, NE_STRING);
cmp_sig!(builtinNETimeSig, 154, true, NE_TIME);
cmp_sig!(builtinNEDurationSig, 155, true, NE_DURATION);
cmp_sig!(builtinNEJSONSig, 156, true, NE_JSON);
cmp_sig!(builtinNEVectorFloat32Sig, 5153, false, NE_VECTOR_FLOAT32);
cmp_sig!(builtinNullEQIntSig, 160, true, NULL_EQ_INT);
cmp_sig!(builtinNullEQRealSig, 161, true, NULL_EQ_REAL);
cmp_sig!(builtinNullEQDecimalSig, 162, true, NULL_EQ_DECIMAL);
cmp_sig!(builtinNullEQStringSig, 163, true, NULL_EQ_STRING);
cmp_sig!(builtinNullEQTimeSig, 164, true, NULL_EQ_TIME);
cmp_sig!(builtinNullEQDurationSig, 165, true, NULL_EQ_DURATION);
cmp_sig!(builtinNullEQJSONSig, 166, true, NULL_EQ_JSON);
cmp_sig!(
    builtinNullEQVectorFloat32Sig,
    5154,
    false,
    NULL_EQ_VECTOR_FLOAT32
);

/// The signatures of one comparison type, in the order LT, LE, GT, GE, EQ, NE, NullEQ.
type SigRow = [&'static SigDesc; 7];
static INT_SIGS: SigRow = [
    &builtinLTIntSig,
    &builtinLEIntSig,
    &builtinGTIntSig,
    &builtinGEIntSig,
    &builtinEQIntSig,
    &builtinNEIntSig,
    &builtinNullEQIntSig,
];
static REAL_SIGS: SigRow = [
    &builtinLTRealSig,
    &builtinLERealSig,
    &builtinGTRealSig,
    &builtinGERealSig,
    &builtinEQRealSig,
    &builtinNERealSig,
    &builtinNullEQRealSig,
];
static DECIMAL_SIGS: SigRow = [
    &builtinLTDecimalSig,
    &builtinLEDecimalSig,
    &builtinGTDecimalSig,
    &builtinGEDecimalSig,
    &builtinEQDecimalSig,
    &builtinNEDecimalSig,
    &builtinNullEQDecimalSig,
];
static STRING_SIGS: SigRow = [
    &builtinLTStringSig,
    &builtinLEStringSig,
    &builtinGTStringSig,
    &builtinGEStringSig,
    &builtinEQStringSig,
    &builtinNEStringSig,
    &builtinNullEQStringSig,
];
static TIME_SIGS: SigRow = [
    &builtinLTTimeSig,
    &builtinLETimeSig,
    &builtinGTTimeSig,
    &builtinGETimeSig,
    &builtinEQTimeSig,
    &builtinNETimeSig,
    &builtinNullEQTimeSig,
];
static DURATION_SIGS: SigRow = [
    &builtinLTDurationSig,
    &builtinLEDurationSig,
    &builtinGTDurationSig,
    &builtinGEDurationSig,
    &builtinEQDurationSig,
    &builtinNEDurationSig,
    &builtinNullEQDurationSig,
];
static JSON_SIGS: SigRow = [
    &builtinLTJSONSig,
    &builtinLEJSONSig,
    &builtinGTJSONSig,
    &builtinGEJSONSig,
    &builtinEQJSONSig,
    &builtinNEJSONSig,
    &builtinNullEQJSONSig,
];
static VECTOR_SIGS: SigRow = [
    &builtinLTVectorFloat32Sig,
    &builtinLEVectorFloat32Sig,
    &builtinGTVectorFloat32Sig,
    &builtinGEVectorFloat32Sig,
    &builtinEQVectorFloat32Sig,
    &builtinNEVectorFloat32Sig,
    &builtinNullEQVectorFloat32Sig,
];
