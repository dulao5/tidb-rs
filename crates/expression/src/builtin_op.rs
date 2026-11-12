//! go-file: pkg/expression/builtin_op.go
//! go-file: pkg/expression/builtin_op_vec.go
//!
//! `builtin_op.go` (D08.5), second batch: the logical operators AND / OR / XOR, NOT,
//! IS TRUE / IS FALSE (with and without keeping NULL) and the unary minus; IS NULL (with
//! constant propagation); the third batch: the bit operators `& | ^ << >> ~`
//! (specs/D08.5-arith).
#![allow(non_upper_case_globals)]

use tidb_base::Error;
use tidb_base::gofmt::Arg;
use tidb_base::mysql::{self, has_unsigned_flag};
use tidb_parser::types::{
    ETDatetime, ETDecimal, ETDuration, ETInt, ETJson, ETReal, ETString, ETTimestamp,
    ETVectorFloat32, EvalType,
};
use tidb_types::errors::ErrOverflow;
use tidb_types::etc::is_type_time;
use tidb_types::json::{BinaryJSON, JSONValue, compare_binary_json, create_binary_json};
use tidb_types::mydecimal::{MyDecimal, decimal_neg};

use crate::ast_funcs as ast;
use crate::builtin::{BaseFunctionClass, new_base_builtin_func_with_tp};
use crate::builtin_registry::FunctionClass;
use crate::constant::Constant;
use crate::context::{BuildContext, EvalContext};
use crate::errors::errJSONInBooleanContext;
use crate::eval::{Args, BuiltinFunc, SigDesc, SigExtra, SigView};
use crate::expression::{Expr, ExprRef};
use tidb_expression_macros::eval_fn;

// ---------------------------------------------------------------- AND / OR / XOR

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LogicKind {
    And,
    Or,
    Xor,
}

/// The function classes of AND, OR and XOR (`getFunction` differs by the signature).
#[derive(Debug)]
pub struct LogicFunctionClass {
    /// The embedded baseFunctionClass.
    pub base: BaseFunctionClass,
    kind: LogicKind,
}

macro_rules! logic_class {
    ($name:ident, $fname:expr, $kind:expr) => {
        #[doc = concat!("The function class of `", stringify!($fname), "`.")]
        pub static $name: LogicFunctionClass = LogicFunctionClass {
            base: BaseFunctionClass {
                func_name: $fname,
                min_args: 2,
                max_args: 2,
            },
            kind: $kind,
        };
    };
}

logic_class!(LOGIC_AND_CLASS, ast::LogicAnd, LogicKind::And);
logic_class!(LOGIC_OR_CLASS, ast::LogicOr, LogicKind::Or);
logic_class!(LOGIC_XOR_CLASS, ast::LogicXor, LogicKind::Xor);

impl FunctionClass for LogicFunctionClass {
    // go: logicAndFunctionClass.getFunction L1
    // go: logicOrFunctionClass.getFunction L1
    // go: logicXorFunctionClass.getFunction L1
    fn get_function(
        &self,
        ctx: &dyn BuildContext,
        mut args: Vec<ExprRef>,
    ) -> Result<BuiltinFunc, Error> {
        self.base.verify_args(&args)?;
        args[0] = crate::expression::wrap_with_is_true(ctx, true, args[0].clone(), false)?;
        args[1] = crate::expression::wrap_with_is_true(ctx, true, args[1].clone(), false)?;

        let mut bf =
            new_base_builtin_func_with_tp(ctx, self.base.func_name, args, ETInt, &[ETInt, ETInt])?;
        bf.tp.set_flen(1);
        let sig: &'static SigDesc = match self.kind {
            LogicKind::And => &builtinLogicAndSig,
            LogicKind::Or => &builtinLogicOrSig,
            LogicKind::Xor => &builtinLogicXorSig,
        };
        Ok(bf.with_sig(sig))
    }
}

/// The row version of AND: `args[1]` is evaluated only when `args[0]` is not false. The
/// vector version evaluates both arguments and falls back to this when `args[1]` fails or
/// warns (`RpnNode::RowFallback`); its merge of the two columns is this body over them.
// go: builtinLogicAndSig.evalInt L1
// go: builtinLogicAndSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinLogicAndSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Int, lazy, args(Int, Int))]
pub fn logic_and(
    ctx: &dyn EvalContext,
    _f: &impl SigView,
    args: &mut impl Args,
) -> Result<Option<i64>, Error> {
    let arg0 = args.int(ctx, 0)?;
    if arg0 == Some(0) {
        return Ok(Some(0));
    }
    let arg1 = args.int(ctx, 1)?;
    if arg1 == Some(0) {
        return Ok(Some(0));
    }
    if arg0.is_none() || arg1.is_none() {
        return Ok(None);
    }
    Ok(Some(1))
}

/// The row version of OR (see [`logic_and`] for the vector version).
// go: builtinLogicOrSig.evalInt L1
// go: builtinLogicOrSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinLogicOrSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Int, lazy, args(Int, Int))]
pub fn logic_or(
    ctx: &dyn EvalContext,
    _f: &impl SigView,
    args: &mut impl Args,
) -> Result<Option<i64>, Error> {
    let arg0 = args.int(ctx, 0)?;
    if arg0.is_some_and(|v| v != 0) {
        return Ok(Some(1));
    }
    let arg1 = args.int(ctx, 1)?;
    if arg1.is_some_and(|v| v != 0) {
        return Ok(Some(1));
    }
    if arg0.is_none() || arg1.is_none() {
        return Ok(None);
    }
    Ok(Some(0))
}

/// XOR: the row version does not evaluate `args[1]` when `args[0]` is NULL; the vector
/// version evaluates both columns (an error or a warning of `args[1]` on such a row only
/// shows with vectorization on; spec §3.2).
// go: builtinLogicXorSig.evalInt L1
// go: builtinLogicXorSig.vecEvalInt L2 (the vector driver of this lazy body over both evaluated columns)
// go: builtinLogicXorSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinLogicXorSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Int, lazy, args(Int, Int))]
pub fn logic_xor(
    ctx: &dyn EvalContext,
    _f: &impl SigView,
    args: &mut impl Args,
) -> Result<Option<i64>, Error> {
    let Some(arg0) = args.int(ctx, 0)? else {
        return Ok(None);
    };
    let Some(arg1) = args.int(ctx, 1)? else {
        return Ok(None);
    };
    if (arg0 != 0 && arg1 != 0) || (arg0 == 0 && arg1 == 0) {
        return Ok(Some(0));
    }
    Ok(Some(1))
}

// ---------------------------------------------------------------- IS TRUE / IS FALSE

/// The function classes of IS TRUE / IS FALSE.
#[derive(Debug)]
pub struct IsTrueOrFalseFunctionClass {
    /// The embedded baseFunctionClass.
    pub base: BaseFunctionClass,
    /// `op == opcode.IsTruth` (else `opcode.IsFalsity`).
    truth: bool,
    /// keepNull indicates how this function treats a null input parameter.
    /// If keepNull is true and the input parameter is null, the function will return null.
    /// If keepNull is false, the null input parameter will be cast to 0.
    keep_null: bool,
}

macro_rules! is_true_or_false_class {
    ($name:ident, $fname:expr, $truth:expr, $keep_null:expr) => {
        #[doc = concat!("The function class of `", stringify!($fname), "`.")]
        pub static $name: IsTrueOrFalseFunctionClass = IsTrueOrFalseFunctionClass {
            base: BaseFunctionClass {
                func_name: $fname,
                min_args: 1,
                max_args: 1,
            },
            truth: $truth,
            keep_null: $keep_null,
        };
    };
}

is_true_or_false_class!(
    IS_TRUTH_WITHOUT_NULL_CLASS,
    ast::IsTruthWithoutNull,
    true,
    false
);
is_true_or_false_class!(IS_TRUTH_WITH_NULL_CLASS, ast::IsTruthWithNull, true, true);
is_true_or_false_class!(IS_FALSITY_CLASS, ast::IsFalsity, false, false);

impl IsTrueOrFalseFunctionClass {
    /// The opcode's text (`opcode.IsTruth` / `opcode.IsFalsity`).
    // go: isTrueOrFalseFunctionClass.getDisplayName L1
    pub fn get_display_name(&self) -> String {
        if self.truth { "IS TRUE" } else { "IS FALSE" }.to_owned()
    }
}

impl FunctionClass for IsTrueOrFalseFunctionClass {
    // go: isTrueOrFalseFunctionClass.getFunction L1
    fn get_function(
        &self,
        ctx: &dyn BuildContext,
        args: Vec<ExprRef>,
    ) -> Result<BuiltinFunc, Error> {
        self.base.verify_args(&args)?;

        let mut arg_tp = args[0].get_type(&*ctx.get_eval_ctx()).eval_type();
        if matches!(
            arg_tp,
            ETTimestamp | ETDatetime | ETDuration | ETJson | ETString
        ) {
            arg_tp = ETReal;
        }

        let mut bf =
            new_base_builtin_func_with_tp(ctx, self.base.func_name, args, ETInt, &[arg_tp])?;
        bf.tp.set_flen(1);
        bf.extra = SigExtra::IsTrueOrFalse {
            keep_null: self.keep_null,
        };

        let sig: &'static SigDesc = match (self.truth, arg_tp) {
            (true, ETReal) => &builtinRealIsTrueSig,
            (true, ETDecimal) => &builtinDecimalIsTrueSig,
            (true, ETInt) => &builtinIntIsTrueSig,
            (true, ETVectorFloat32) => &builtinVectorFloat32IsTrueSig,
            (false, ETReal) => &builtinRealIsFalseSig,
            (false, ETDecimal) => &builtinDecimalIsFalseSig,
            (false, ETInt) => &builtinIntIsFalseSig,
            (false, ETVectorFloat32) => &builtinVectorFloat32IsFalseSig,
            _ => return Err(unexpected_eval_type(arg_tp)),
        };
        let mut f = bf.with_sig(sig);
        if self.keep_null && arg_tp != ETVectorFloat32 {
            // ScalarFuncSig_XxxIsTrueWithNull / XxxIsFalseWithNull.
            f.pb_code += 20;
        }
        Ok(f)
    }
}

/// `errors.Errorf("unexpected types.EvalType %v", argTp)`.
// glue: the error text of isTrueOrFalseFunctionClass.getFunction
fn unexpected_eval_type(tp: EvalType) -> Error {
    Error::plain(format!("unexpected types.EvalType {}", tp.to_string()))
}

/// The body of the IS TRUE / IS FALSE signatures: NULL is NULL with keepNull, else false;
/// `zero` tells whether the value is zero (IS TRUE is `!zero`, IS FALSE `zero`).
#[inline(always)]
// glue: the shared prologue of the IS TRUE / IS FALSE evalInt bodies
fn truth(keep_null: bool, zero: Option<bool>, want_zero: bool) -> Option<i64> {
    match zero {
        None if keep_null => None,
        None => Some(0),
        Some(z) => Some(i64::from(z == want_zero)),
    }
}

macro_rules! is_true_or_false {
    ($($(#[$m:meta])* $fname:ident, $tp:ident, $get:ident, $want_zero:expr, |$v:ident| $zero:expr);* $(;)?) => {$(
        $(#[$m])*
        #[eval_fn(Int, lazy, args($tp))]
        pub fn $fname(
            ctx: &dyn EvalContext,
            f: &impl SigView,
            args: &mut impl Args,
        ) -> Result<Option<i64>, Error> {
            let zero = args.$get(ctx, 0)?.map(|$v| $zero);
            Ok(truth(f.keep_null(), zero, $want_zero))
        }
    )*};
}

is_true_or_false! {
    // go: builtinRealIsTrueSig.evalInt L1
    // go: builtinRealIsTrueSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
    // go: builtinRealIsTrueSig.vectorized L2 (SigDesc.vectorized)
    // go: builtinRealIsTrueSig.Clone L2 (BuiltinFunc::clone_from_base copies SigExtra::IsTrueOrFalse, as Go)
    real_is_true, Real, real, false, |v| v == 0.0;
    // go: builtinDecimalIsTrueSig.evalInt L1
    // go: builtinDecimalIsTrueSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
    // go: builtinDecimalIsTrueSig.vectorized L2 (SigDesc.vectorized)
    // go: builtinDecimalIsTrueSig.Clone L2 (BuiltinFunc::clone_from_base copies SigExtra::IsTrueOrFalse, as Go)
    decimal_is_true, Decimal, decimal, false, |v| v.is_zero();
    // go: builtinIntIsTrueSig.evalInt L1
    // go: builtinIntIsTrueSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
    // go: builtinIntIsTrueSig.vectorized L2 (SigDesc.vectorized)
    // go: builtinIntIsTrueSig.Clone L2 (BuiltinFunc::clone_from_base copies SigExtra::IsTrueOrFalse, as Go)
    int_is_true, Int, int, false, |v| v == 0;
    // go: builtinRealIsFalseSig.evalInt L1
    // go: builtinRealIsFalseSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
    // go: builtinRealIsFalseSig.vectorized L2 (SigDesc.vectorized)
    // go: builtinRealIsFalseSig.Clone L2 (BuiltinFunc::clone_from_base copies SigExtra::IsTrueOrFalse, as Go)
    real_is_false, Real, real, true, |v| v == 0.0;
    // go: builtinDecimalIsFalseSig.evalInt L1
    // go: builtinDecimalIsFalseSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
    // go: builtinDecimalIsFalseSig.vectorized L2 (SigDesc.vectorized)
    // go: builtinDecimalIsFalseSig.Clone L2 (BuiltinFunc::clone_from_base copies SigExtra::IsTrueOrFalse, as Go)
    decimal_is_false, Decimal, decimal, true, |v| v.is_zero();
    // go: builtinIntIsFalseSig.evalInt L1
    // go: builtinIntIsFalseSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
    // go: builtinIntIsFalseSig.vectorized L2 (SigDesc.vectorized)
    // go: builtinIntIsFalseSig.Clone L2 (BuiltinFunc::clone_from_base copies SigExtra::IsTrueOrFalse, as Go)
    int_is_false, Int, int, true, |v| v == 0;
}

/// IS TRUE over a vector (Go has no vector version).
// go: builtinVectorFloat32IsTrueSig.evalInt L1
// go: builtinVectorFloat32IsTrueSig.Clone L2 (BuiltinFunc::clone_from_base copies SigExtra::IsTrueOrFalse, as Go)
#[eval_fn(Int, lazy, novec)]
pub fn vector_float32_is_true(
    ctx: &dyn EvalContext,
    f: &impl SigView,
    args: &mut impl Args,
) -> Result<Option<i64>, Error> {
    let zero = args.vector_float32(ctx, 0)?.map(|v| v.is_zero_value());
    Ok(truth(f.keep_null(), zero, false))
}

/// IS FALSE over a vector (Go has no vector version).
// go: builtinVectorFloat32IsFalseSig.evalInt L1
// go: builtinVectorFloat32IsFalseSig.Clone L2 (BuiltinFunc::clone_from_base copies SigExtra::IsTrueOrFalse, as Go)
#[eval_fn(Int, lazy, novec)]
pub fn vector_float32_is_false(
    ctx: &dyn EvalContext,
    f: &impl SigView,
    args: &mut impl Args,
) -> Result<Option<i64>, Error> {
    let zero = args.vector_float32(ctx, 0)?.map(|v| v.is_zero_value());
    Ok(truth(f.keep_null(), zero, true))
}

// ---------------------------------------------------------------- NOT

/// The function class of NOT (`!`).
#[derive(Debug)]
pub struct UnaryNotFunctionClass {
    /// The embedded baseFunctionClass.
    pub base: BaseFunctionClass,
}

/// The function class of `not`.
pub static UNARY_NOT_CLASS: UnaryNotFunctionClass = UnaryNotFunctionClass {
    base: BaseFunctionClass {
        func_name: ast::UnaryNot,
        min_args: 1,
        max_args: 1,
    },
};

impl FunctionClass for UnaryNotFunctionClass {
    // go: unaryNotFunctionClass.getFunction L1
    fn get_function(
        &self,
        ctx: &dyn BuildContext,
        args: Vec<ExprRef>,
    ) -> Result<BuiltinFunc, Error> {
        self.base.verify_args(&args)?;

        let ev = ctx.get_eval_ctx();
        let mut arg_tp = args[0].get_type(&*ev).eval_type();
        if matches!(arg_tp, ETTimestamp | ETDatetime | ETDuration) {
            arg_tp = ETInt;
        } else if arg_tp == ETString {
            arg_tp = ETReal;
        }

        let mut bf =
            new_base_builtin_func_with_tp(ctx, self.base.func_name, args, ETInt, &[arg_tp])?;
        bf.tp.set_flen(1);

        let sig: &'static SigDesc = match arg_tp {
            ETReal => &builtinUnaryNotRealSig,
            ETDecimal => &builtinUnaryNotDecimalSig,
            ETInt => &builtinUnaryNotIntSig,
            ETJson => {
                ev.append_warning((*errJSONInBooleanContext).clone());
                &builtinUnaryNotJSONSig
            }
            _ => {
                return Err(Error::plain(format!(
                    "{} is not supported for unary not operator",
                    arg_tp.to_string()
                )));
            }
        };
        Ok(bf.with_sig(sig))
    }
}

// go: builtinUnaryNotRealSig.evalInt L1
// go: builtinUnaryNotRealSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
// go: builtinUnaryNotRealSig.vectorized L2 (SigDesc.vectorized)
// go: builtinUnaryNotRealSig.Clone L2 (BuiltinFunc::clone_from_base)
#[eval_fn(Int)]
pub fn unary_not_real(
    _ctx: &dyn EvalContext,
    _f: &impl SigView,
    arg: f64,
) -> Result<Option<i64>, Error> {
    Ok(Some(i64::from(arg == 0.0)))
}

// go: builtinUnaryNotDecimalSig.evalInt L1
// go: builtinUnaryNotDecimalSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
// go: builtinUnaryNotDecimalSig.vectorized L2 (SigDesc.vectorized)
// go: builtinUnaryNotDecimalSig.Clone L2 (BuiltinFunc::clone_from_base)
#[eval_fn(Int)]
pub fn unary_not_decimal(
    _ctx: &dyn EvalContext,
    _f: &impl SigView,
    arg: &MyDecimal,
) -> Result<Option<i64>, Error> {
    Ok(Some(i64::from(arg.is_zero())))
}

// go: builtinUnaryNotIntSig.evalInt L1
// go: builtinUnaryNotIntSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
// go: builtinUnaryNotIntSig.vectorized L2 (SigDesc.vectorized)
// go: builtinUnaryNotIntSig.Clone L2 (BuiltinFunc::clone_from_base)
#[eval_fn(Int)]
pub fn unary_not_int(
    _ctx: &dyn EvalContext,
    _f: &impl SigView,
    arg: i64,
) -> Result<Option<i64>, Error> {
    Ok(Some(i64::from(arg == 0)))
}

/// NOT over JSON (Go has no vector version).
// go: builtinUnaryNotJSONSig.evalInt L1
// go: builtinUnaryNotJSONSig.Clone L2 (BuiltinFunc::clone_from_base)
#[eval_fn(Int, novec)]
pub fn unary_not_json(
    _ctx: &dyn EvalContext,
    _f: &impl SigView,
    arg: &BinaryJSON<'_>,
) -> Result<Option<i64>, Error> {
    let zero = create_binary_json(JSONValue::Int64(0));
    Ok(Some(i64::from(compare_binary_json(arg.clone(), zero) == 0)))
}

// ---------------------------------------------------------------- IS NULL

/// The function class of IS NULL.
#[derive(Debug)]
pub struct IsNullFunctionClass {
    /// The embedded baseFunctionClass.
    pub base: BaseFunctionClass,
}

/// The function class of `isnull`.
pub static IS_NULL_CLASS: IsNullFunctionClass = IsNullFunctionClass {
    base: BaseFunctionClass {
        func_name: ast::IsNull,
        min_args: 1,
        max_args: 1,
    },
};

impl FunctionClass for IsNullFunctionClass {
    // go: isNullFunctionClass.getFunction L1
    fn get_function(
        &self,
        ctx: &dyn BuildContext,
        args: Vec<ExprRef>,
    ) -> Result<BuiltinFunc, Error> {
        self.base.verify_args(&args)?;
        let ev = ctx.get_eval_ctx();
        let mut arg_tp = args[0].get_type(&*ev).eval_type();
        if arg_tp == ETTimestamp {
            arg_tp = ETDatetime;
        } else if arg_tp == ETJson {
            arg_tp = ETString;
        }
        let mut bf =
            new_base_builtin_func_with_tp(ctx, self.base.func_name, args, ETInt, &[arg_tp])?;
        bf.tp.set_flen(1);
        let sig: &'static SigDesc = match arg_tp {
            ETInt => &builtinIntIsNullSig,
            ETDecimal => &builtinDecimalIsNullSig,
            ETReal => &builtinRealIsNullSig,
            ETDatetime => &builtinTimeIsNullSig,
            ETDuration => &builtinDurationIsNullSig,
            ETString => &builtinStringIsNullSig,
            ETVectorFloat32 => &builtinVectorFloat32IsNullSig,
            _ => {
                return Err(Error::plain(format!(
                    "{} is not supported for ISNULL()",
                    arg_tp.to_string()
                )));
            }
        };
        Ok(bf.with_sig(sig))
    }
}

/// `evalIsNull`: 1 when the argument is NULL; an error of the argument is returned.
// go: evalIsNull L1
fn eval_is_null<T>(r: Result<Option<T>, Error>) -> Result<Option<i64>, Error> {
    Ok(Some(i64::from(r?.is_none())))
}

/// IS NULL over each argument type (lazy bodies: the argument may be NULL).
macro_rules! is_null_sig {
    ($($(#[$m:meta])* $fname:ident, $get:ident, [$($opt:ident),*]);* $(;)?) => {$(
        $(#[$m])*
        #[eval_fn(Int, lazy $(, $opt)*)]
        pub fn $fname(
            ctx: &dyn EvalContext,
            _f: &impl SigView,
            args: &mut impl Args,
        ) -> Result<Option<i64>, Error> {
            eval_is_null(args.$get(ctx, 0))
        }
    )*};
}

is_null_sig!(
    // go: builtinIntIsNullSig.evalInt L1
    // go: builtinIntIsNullSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
    // go: builtinIntIsNullSig.vectorized L2 (SigDesc.vectorized)
    // go: builtinIntIsNullSig.Clone L2 (BuiltinFunc::clone_from_base)
    int_is_null, int, [];
    // go: builtinDecimalIsNullSig.evalInt L1
    // go: builtinDecimalIsNullSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
    // go: builtinDecimalIsNullSig.vectorized L2 (SigDesc.vectorized)
    // go: builtinDecimalIsNullSig.Clone L2 (BuiltinFunc::clone_from_base)
    decimal_is_null, decimal, [];
    // go: builtinRealIsNullSig.evalInt L1
    // go: builtinRealIsNullSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
    // go: builtinRealIsNullSig.vectorized L2 (SigDesc.vectorized)
    // go: builtinRealIsNullSig.Clone L2 (BuiltinFunc::clone_from_base)
    real_is_null, real, [];
    // go: builtinTimeIsNullSig.evalInt L1
    // go: builtinTimeIsNullSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
    // go: builtinTimeIsNullSig.vectorized L2 (SigDesc.vectorized)
    // go: builtinTimeIsNullSig.Clone L2 (BuiltinFunc::clone_from_base)
    time_is_null, time, [];
    // go: builtinDurationIsNullSig.evalInt L1
    // go: builtinDurationIsNullSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
    // go: builtinDurationIsNullSig.vectorized L2 (SigDesc.vectorized)
    // go: builtinDurationIsNullSig.Clone L2 (BuiltinFunc::clone_from_base)
    duration_is_null, duration, [];
    // go: builtinStringIsNullSig.evalInt L1
    // go: pkg/expression/builtin_string_vec.go builtinStringIsNullSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
    // go: pkg/expression/builtin_string_vec.go builtinStringIsNullSig.vectorized L2 (SigDesc.vectorized)
    // go: builtinStringIsNullSig.Clone L2 (BuiltinFunc::clone_from_base)
    string_is_null, string, [];
    /// IS NULL over VECTOR (Go has no vector version).
    // go: builtinVectorFloat32IsNullSig.evalInt L1
    // go: builtinVectorFloat32IsNullSig.Clone L2 (BuiltinFunc::clone_from_base)
    vector_float32_is_null, vector_float32, [novec];
);

// ---------------------------------------------------------------- unary minus

/// The function class of the unary minus.
#[derive(Debug)]
pub struct UnaryMinusFunctionClass {
    /// The embedded baseFunctionClass.
    pub base: BaseFunctionClass,
}

/// The function class of `unaryminus`.
pub static UNARY_MINUS_CLASS: UnaryMinusFunctionClass = UnaryMinusFunctionClass {
    base: BaseFunctionClass {
        func_name: ast::UnaryMinus,
        min_args: 1,
        max_args: 1,
    },
};

impl UnaryMinusFunctionClass {
    // go: unaryMinusFunctionClass.handleIntOverflow L1
    fn handle_int_overflow(&self, ctx: &dyn EvalContext, arg: &Constant) -> bool {
        let unsigned = arg
            .get_type(ctx)
            .is_some_and(|tp| has_unsigned_flag(tp.get_flag()));
        if unsigned {
            let uval = arg.value.get_uint64();
            // -math.MinInt64 is 9223372036854775808, so if uval is more than 9223372036854775808, like
            // 9223372036854775809, -9223372036854775809 is less than math.MinInt64, overflow occurs.
            if uval > i64::MIN.unsigned_abs() {
                return true;
            }
        } else {
            let val = arg.value.get_int64();
            // The math.MinInt64 is -9223372036854775808, the math.MaxInt64 is 9223372036854775807,
            // which is less than abs(-9223372036854775808). When val == math.MinInt64, overflow occurs.
            if val == i64::MIN {
                return true;
            }
        }
        false
    }

    /// typeInfer infers unaryMinus function return type. when the arg is an int constant and
    /// overflow, typerInfer will infers the return type as types.ETDecimal, not types.ETInt.
    // go: unaryMinusFunctionClass.typeInfer L1
    fn type_infer(&self, ctx: &dyn EvalContext, arg_expr: &Expr) -> (EvalType, bool) {
        let mut tp = arg_expr.get_type(ctx).eval_type();
        if tp != ETInt && tp != ETDecimal {
            tp = ETReal;
        }

        let mut overflow = false;
        // TODO: Handle float overflow.
        if let Expr::Constant(arg) = arg_expr
            && tp == ETInt
        {
            overflow = self.handle_int_overflow(ctx, arg);
            if overflow {
                tp = ETDecimal;
            }
        }
        (tp, overflow)
    }
}

impl FunctionClass for UnaryMinusFunctionClass {
    // go: unaryMinusFunctionClass.getFunction L1
    fn get_function(
        &self,
        ctx: &dyn BuildContext,
        args: Vec<ExprRef>,
    ) -> Result<BuiltinFunc, Error> {
        self.base.verify_args(&args)?;

        let ev = ctx.get_eval_ctx();
        // Go keeps `args[0].GetType()` from before newBaseBuiltinFuncWithTp; no PropagateType
        // reaches it here (only an ETReal wrap of a DECIMAL argument changes its type, and a
        // DECIMAL argument is not wrapped as REAL).
        let arg_expr_tp = args[0].get_type(&*ev).into_owned();
        let (_, int_overflow) = self.type_infer(&*ev, &args[0]);

        let eval_type = arg_expr_tp.eval_type();
        let (mut bf, sig): (BuiltinFunc, &'static SigDesc) = match eval_type {
            ETInt => {
                let (mut bf, sig) = if int_overflow {
                    let mut bf = new_base_builtin_func_with_tp(
                        ctx,
                        self.base.func_name,
                        args,
                        ETDecimal,
                        &[ETDecimal],
                    )?;
                    bf.extra = SigExtra::UnaryMinusDecimal {
                        constant_arg_overflow: true,
                    };
                    (bf, &builtinUnaryMinusDecimalSig)
                } else {
                    let bf = new_base_builtin_func_with_tp(
                        ctx,
                        self.base.func_name,
                        args,
                        ETInt,
                        &[ETInt],
                    )?;
                    (bf, &builtinUnaryMinusIntSig)
                };
                bf.tp.set_decimal(0);
                (bf, sig)
            }
            ETReal => {
                let bf = new_base_builtin_func_with_tp(
                    ctx,
                    self.base.func_name,
                    args,
                    ETReal,
                    &[ETReal],
                )?;
                (bf, &builtinUnaryMinusRealSig)
            }
            _ => {
                let mut as_decimal = eval_type == ETDecimal;
                if !as_decimal {
                    let tp = arg_expr_tp.get_type();
                    if is_type_time(tp) || tp == mysql::TypeDuration {
                        as_decimal = true;
                    }
                }
                if as_decimal {
                    let mut bf = new_base_builtin_func_with_tp(
                        ctx,
                        self.base.func_name,
                        args,
                        ETDecimal,
                        &[ETDecimal],
                    )?;
                    bf.tp.set_decimal_under_limit(arg_expr_tp.get_decimal());
                    bf.extra = SigExtra::UnaryMinusDecimal {
                        constant_arg_overflow: false,
                    };
                    (bf, &builtinUnaryMinusDecimalSig)
                } else {
                    let bf = new_base_builtin_func_with_tp(
                        ctx,
                        self.base.func_name,
                        args,
                        ETReal,
                        &[ETReal],
                    )?;
                    (bf, &builtinUnaryMinusRealSig)
                }
            }
        };
        bf.tp.set_flen_under_limit(arg_expr_tp.get_flen() + 1);
        Ok(bf.with_sig(sig))
    }
}

/// The negation of a BIGINT (unsigned: up to 9223372036854775808).
// go: builtinUnaryMinusIntSig.evalInt L1
// go: builtinUnaryMinusIntSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
// go: builtinUnaryMinusIntSig.vectorized L2 (SigDesc.vectorized)
// go: builtinUnaryMinusIntSig.Clone L2 (BuiltinFunc::clone_from_base)
#[eval_fn(Int)]
pub fn unary_minus_int(
    _ctx: &dyn EvalContext,
    f: &impl SigView,
    val: i64,
) -> Result<Option<i64>, Error> {
    if has_unsigned_flag(f.arg_type(0).get_flag()) {
        let uval = val as u64;
        if uval > i64::MIN.unsigned_abs() {
            return Err(ErrOverflow
                .gen_with_stack_by_args(vec![Arg::from("BIGINT"), Arg::from(format!("-{uval}"))]));
        } else if uval == i64::MIN.unsigned_abs() {
            return Ok(Some(i64::MIN));
        }
    } else if val == i64::MIN {
        return Err(ErrOverflow
            .gen_with_stack_by_args(vec![Arg::from("BIGINT"), Arg::from(format!("-{val}"))]));
    }
    Ok(Some(val.wrapping_neg()))
}

// go: builtinUnaryMinusDecimalSig.evalDecimal L1
// go: builtinUnaryMinusDecimalSig.vecEvalDecimal L2 (the vector driver of this body, M-EVAL)
// go: builtinUnaryMinusDecimalSig.vectorized L2 (SigDesc.vectorized)
// go: builtinUnaryMinusDecimalSig.Clone L2 (BuiltinFunc::clone_from_base copies SigExtra::UnaryMinusDecimal, as Go)
#[eval_fn(Decimal)]
pub fn unary_minus_decimal(
    _ctx: &dyn EvalContext,
    _f: &impl SigView,
    dec: &MyDecimal,
) -> Result<Option<MyDecimal>, Error> {
    Ok(Some(decimal_neg(dec)))
}

// go: builtinUnaryMinusRealSig.evalReal L1
// go: builtinUnaryMinusRealSig.vecEvalReal L2 (the vector driver of this body, M-EVAL)
// go: builtinUnaryMinusRealSig.vectorized L2 (SigDesc.vectorized)
// go: builtinUnaryMinusRealSig.Clone L2 (BuiltinFunc::clone_from_base)
#[eval_fn(Real)]
pub fn unary_minus_real(
    _ctx: &dyn EvalContext,
    _f: &impl SigView,
    val: f64,
) -> Result<Option<f64>, Error> {
    Ok(Some(-val))
}

// ---------------------------------------------------------------- bit operators (batch 3)

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BitKind {
    And,
    Or,
    Xor,
    LeftShift,
    RightShift,
}

/// The function classes of `& | ^ << >>` (`bitAndFunctionClass`, `bitOrFunctionClass`,
/// `bitXorFunctionClass`, `leftShiftFunctionClass`, `rightShiftFunctionClass`): they
/// differ only by the signature.
#[derive(Debug)]
pub struct BitBinaryFunctionClass {
    /// The embedded baseFunctionClass.
    pub base: BaseFunctionClass,
    kind: BitKind,
}

macro_rules! bit_class {
    ($name:ident, $fname:expr, $kind:expr) => {
        #[doc = concat!("The function class of `", stringify!($fname), "`.")]
        pub static $name: BitBinaryFunctionClass = BitBinaryFunctionClass {
            base: BaseFunctionClass {
                func_name: $fname,
                min_args: 2,
                max_args: 2,
            },
            kind: $kind,
        };
    };
}

bit_class!(BIT_AND_CLASS, ast::And, BitKind::And);
bit_class!(BIT_OR_CLASS, ast::Or, BitKind::Or);
bit_class!(BIT_XOR_CLASS, ast::Xor, BitKind::Xor);
bit_class!(LEFT_SHIFT_CLASS, ast::LeftShift, BitKind::LeftShift);
bit_class!(RIGHT_SHIFT_CLASS, ast::RightShift, BitKind::RightShift);

impl FunctionClass for BitBinaryFunctionClass {
    // go: bitAndFunctionClass.getFunction L1
    // go: bitOrFunctionClass.getFunction L1
    // go: bitXorFunctionClass.getFunction L1
    // go: leftShiftFunctionClass.getFunction L1
    // go: rightShiftFunctionClass.getFunction L1
    fn get_function(
        &self,
        ctx: &dyn BuildContext,
        args: Vec<ExprRef>,
    ) -> Result<BuiltinFunc, Error> {
        self.base.verify_args(&args)?;
        let mut bf =
            new_base_builtin_func_with_tp(ctx, self.base.func_name, args, ETInt, &[ETInt, ETInt])?;
        let sig: &'static SigDesc = match self.kind {
            BitKind::And => &builtinBitAndSig,
            BitKind::Or => &builtinBitOrSig,
            BitKind::Xor => &builtinBitXorSig,
            BitKind::LeftShift => &builtinLeftShiftSig,
            BitKind::RightShift => &builtinRightShiftSig,
        };
        // Go sets the flag on sig.tp after setPbCode; RetType and sig.tp are one object.
        bf.tp.add_flag(mysql::UnsignedFlag);
        Ok(bf.with_sig(sig))
    }
}

// The row versions do not evaluate `args[1]` when `args[0]` is NULL; the vector versions
// evaluate both columns and merge the NULLs (an error or a warning of `args[1]` on such a
// row only shows with vectorization on; spec §3.2, like LogicXor). So the bodies are lazy:
// the vector driver evaluates both columns and runs the body over them.
macro_rules! bit_binary {
    ($fn:ident, $go:literal, |$a:ident, $b:ident| $e:expr) => {
        #[doc = concat!("`", $go, "`.")]
        #[eval_fn(Int, lazy, args(Int, Int))]
        pub fn $fn(
            ctx: &dyn EvalContext,
            _f: &impl SigView,
            args: &mut impl Args,
        ) -> Result<Option<i64>, Error> {
            let Some($a) = args.int(ctx, 0)? else {
                return Ok(None);
            };
            let Some($b) = args.int(ctx, 1)? else {
                return Ok(None);
            };
            Ok(Some($e))
        }
    };
}

// go: builtinBitAndSig.evalInt L1
// go: builtinBitAndSig.vecEvalInt L2 (the vector driver of this lazy body over both evaluated columns)
// go: builtinBitAndSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinBitAndSig.vectorized L2 (SigDesc.vectorized)
bit_binary!(bit_and, "builtinBitAndSig", |a, b| a & b);
// go: builtinBitOrSig.evalInt L1
// go: builtinBitOrSig.vecEvalInt L2 (the vector driver of this lazy body over both evaluated columns)
// go: builtinBitOrSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinBitOrSig.vectorized L2 (SigDesc.vectorized)
bit_binary!(bit_or, "builtinBitOrSig", |a, b| a | b);
// go: builtinBitXorSig.evalInt L1
// go: builtinBitXorSig.vecEvalInt L2 (the vector driver of this lazy body over both evaluated columns)
// go: builtinBitXorSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinBitXorSig.vectorized L2 (SigDesc.vectorized)
bit_binary!(bit_xor, "builtinBitXorSig", |a, b| a ^ b);
// Go's `uint64(a) << uint64(b)` is 0 for a shift count >= 64 (a negative count is a huge
// uint64); Rust's `<<` would overflow, so the count is checked.
// go: builtinLeftShiftSig.evalInt L1
// go: builtinLeftShiftSig.vecEvalInt L2 (the vector driver of this lazy body over both evaluated columns)
// go: builtinLeftShiftSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinLeftShiftSig.vectorized L2 (SigDesc.vectorized)
bit_binary!(left_shift, "builtinLeftShiftSig", |a, b| {
    if (b as u64) < 64 {
        ((a as u64) << b) as i64
    } else {
        0
    }
});
// go: builtinRightShiftSig.evalInt L1
// go: builtinRightShiftSig.vecEvalInt L2 (the vector driver of this lazy body over both evaluated columns)
// go: builtinRightShiftSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinRightShiftSig.vectorized L2 (SigDesc.vectorized)
bit_binary!(right_shift, "builtinRightShiftSig", |a, b| {
    if (b as u64) < 64 {
        ((a as u64) >> b) as i64
    } else {
        0
    }
});

/// The function class of `~` (`bitNegFunctionClass`).
#[derive(Debug)]
pub struct BitNegFunctionClass {
    /// The embedded baseFunctionClass.
    pub base: BaseFunctionClass,
}

/// The function class of `bitneg`.
pub static BIT_NEG_CLASS: BitNegFunctionClass = BitNegFunctionClass {
    base: BaseFunctionClass {
        func_name: ast::BitNeg,
        min_args: 1,
        max_args: 1,
    },
};

impl FunctionClass for BitNegFunctionClass {
    // go: bitNegFunctionClass.getFunction L1
    fn get_function(
        &self,
        ctx: &dyn BuildContext,
        args: Vec<ExprRef>,
    ) -> Result<BuiltinFunc, Error> {
        self.base.verify_args(&args)?;
        let mut bf =
            new_base_builtin_func_with_tp(ctx, self.base.func_name, args, ETInt, &[ETInt])?;
        bf.tp.add_flag(mysql::UnsignedFlag);
        Ok(bf.with_sig(&builtinBitNegSig))
    }
}

// go: builtinBitNegSig.evalInt L1
// go: builtinBitNegSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
// go: builtinBitNegSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinBitNegSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Int)]
pub fn bit_neg(_ctx: &dyn EvalContext, _f: &impl SigView, arg: i64) -> Result<Option<i64>, Error> {
    Ok(Some(!arg))
}

// ---------------------------------------------------------------- signature descriptors

macro_rules! op_sig {
    ($name:ident, $pb:expr, $vectorized:expr, $drivers:ident) => {
        op_sig!($name, $pb, $vectorized, $drivers, None);
    };
    ($name:ident, $pb:expr, $vectorized:expr, $drivers:ident, $fallback:ident) => {
        #[doc = concat!("`", stringify!($name), "`.")]
        pub static $name: SigDesc = SigDesc {
            name: stringify!($name),
            pb_code: $pb,
            vectorized: $vectorized,
            drivers: $drivers,
            args_fallback: crate::eval::ArgsFallback::$fallback,
        };
    };
}

// tipb.ScalarFuncSig codes (go-tipb expression.pb.go); the IS TRUE / IS FALSE signatures
// built with keepNull get the `...WithNull` code (+20, set in getFunction); the VECTOR
// IS TRUE / IS FALSE signatures have no code (Go comments the setPbCode out).
op_sig!(builtinLogicAndSig, 3101, true, LOGIC_AND, SecondArg);
op_sig!(builtinLogicOrSig, 3102, true, LOGIC_OR, SecondArg);
op_sig!(builtinLogicXorSig, 3103, true, LOGIC_XOR);
op_sig!(builtinUnaryNotRealSig, 3106, true, UNARY_NOT_REAL);
op_sig!(builtinUnaryNotDecimalSig, 3105, true, UNARY_NOT_DECIMAL);
op_sig!(builtinUnaryNotIntSig, 3104, true, UNARY_NOT_INT);
op_sig!(builtinUnaryNotJSONSig, 3107, false, UNARY_NOT_JSON);
op_sig!(builtinUnaryMinusIntSig, 3108, true, UNARY_MINUS_INT);
op_sig!(builtinUnaryMinusRealSig, 3109, true, UNARY_MINUS_REAL);
op_sig!(builtinUnaryMinusDecimalSig, 3110, true, UNARY_MINUS_DECIMAL);
op_sig!(builtinRealIsTrueSig, 3123, true, REAL_IS_TRUE);
op_sig!(builtinDecimalIsTrueSig, 3124, true, DECIMAL_IS_TRUE);
op_sig!(builtinIntIsTrueSig, 3122, true, INT_IS_TRUE);
op_sig!(builtinRealIsFalseSig, 3126, true, REAL_IS_FALSE);
op_sig!(builtinDecimalIsFalseSig, 3127, true, DECIMAL_IS_FALSE);
op_sig!(builtinIntIsFalseSig, 3125, true, INT_IS_FALSE);
op_sig!(
    builtinVectorFloat32IsTrueSig,
    0,
    false,
    VECTOR_FLOAT32_IS_TRUE
);
op_sig!(
    builtinVectorFloat32IsFalseSig,
    0,
    false,
    VECTOR_FLOAT32_IS_FALSE
);
op_sig!(builtinIntIsNullSig, 3116, true, INT_IS_NULL);
op_sig!(builtinDecimalIsNullSig, 3111, true, DECIMAL_IS_NULL);
op_sig!(builtinRealIsNullSig, 3113, true, REAL_IS_NULL);
op_sig!(builtinTimeIsNullSig, 3115, true, TIME_IS_NULL);
op_sig!(builtinDurationIsNullSig, 3112, true, DURATION_IS_NULL);
op_sig!(builtinStringIsNullSig, 3114, true, STRING_IS_NULL);
op_sig!(
    builtinVectorFloat32IsNullSig,
    5155,
    false,
    VECTOR_FLOAT32_IS_NULL
);
op_sig!(builtinBitAndSig, 3118, true, BIT_AND);
op_sig!(builtinBitOrSig, 3119, true, BIT_OR);
op_sig!(builtinBitXorSig, 3120, true, BIT_XOR);
op_sig!(builtinBitNegSig, 3121, true, BIT_NEG);
op_sig!(builtinLeftShiftSig, 3129, true, LEFT_SHIFT);
op_sig!(builtinRightShiftSig, 3130, true, RIGHT_SHIFT);
