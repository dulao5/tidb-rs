//! go-file: pkg/expression/builtin_math.go
//! go-file: pkg/expression/builtin_math_vec.go
//!
//! `builtin_math.go` (D08.5). Batch 1: CEIL / CEILING and FLOOR (the two function classes and
//! their ten signatures; `RefineComparedConstant` (D08.4) builds them through
//! `NewFunctionInternal`). Batch 4: ABS, ROUND, TRUNCATE, SIGN, SQRT, POW / POWER, EXP,
//! LN / LOG / LOG2 / LOG10, PI, DEGREES / RADIANS, SIN / COS / TAN / COT, ASIN / ACOS /
//! ATAN / ATAN2 and RAND. CONV and CRC32 (string arguments) are a later batch
//! (specs/D08.5-arith). The `math` functions are Go's, bit for bit
//! (`tidb_base::gostd::math`: the per-target assembly of `Exp` / `Log` and the fused
//! multiply-adds of arm64).
#![allow(non_upper_case_globals)]

use std::sync::Arc;

use tidb_base::Error;
use tidb_base::errors::RedactLogDisable;
use tidb_base::gofmt::Arg;
use tidb_base::gostd::bytes::trim_space;
use tidb_base::gostd::conv::{f64_to_i64, f64_to_u64};
use tidb_base::gostd::crc32::checksum_ieee;
use tidb_base::gostd::math as gomath;
use tidb_base::gostd::strconv::format_float;
use tidb_base::mysql::{
    self, MaxDecimalScale, MaxIntWidth, TypeDouble, TypeLong, TypeLonglong, TypeNewDecimal,
};
use tidb_chunk::Row;
use tidb_parser::types::{ETDecimal, ETInt, ETReal, ETString, EvalType, UnspecifiedLength};
use tidb_sessionctx::variable::session_state::MysqlRng;
use tidb_types::datum::{KindBinaryLiteral, new_string_datum};
use tidb_types::errors::{ErrOverflow, ErrTruncated};
use tidb_types::etc::FieldType;
use tidb_types::helper;
use tidb_types::mydecimal::{
    MODE_HALF_UP, MODE_TRUNCATE, MyDecimal, decimal_add, decimal_sub, new_dec_from_int,
    new_dec_from_uint,
};

use crate::ast_funcs as ast;
use crate::builtin::{BaseFunctionClass, new_base_builtin_func_with_tp};
use crate::builtin_registry::FunctionClass;
use crate::context::{BuildContext, EvalContext, type_ctx};
use crate::errors::ErrInvalidArgumentForLogarithm;
use crate::eval::{Args, BuiltinFunc, SigDesc, SigDrivers, SigExtra, SigView};
use crate::expression::{EvalState, Expr, ExprRef, is_binary_literal};
use tidb_expression_macros::eval_fn;

/// getEvalTp4FloorAndCeil gets the types.EvalType of FLOOR and CEIL.
// go: getEvalTp4FloorAndCeil L1
pub fn get_eval_tp4_floor_and_ceil(ctx: &dyn EvalContext, arg: &Expr) -> (EvalType, EvalType) {
    let field_tp = arg.get_type(ctx);
    let (mut ret_tp, mut arg_tp) = (ETInt, field_tp.eval_type());
    match arg_tp {
        ETInt => ret_tp = ETInt,
        ETDecimal => {
            if field_tp.get_flen() - field_tp.get_decimal() > MaxIntWidth - 2 {
                // len(math.MaxInt64) - 1
                ret_tp = ETDecimal;
            }
        }
        _ => {
            ret_tp = ETReal;
            arg_tp = ETReal;
        }
    }
    (ret_tp, arg_tp)
}

/// setFlag4FloorAndCeil sets return flag of FLOOR and CEIL.
// go: setFlag4FloorAndCeil L1
pub fn set_flag4_floor_and_ceil(ctx: &dyn EvalContext, tp: &mut FieldType, arg: &Expr) {
    let field_tp = arg.get_type(ctx);
    if (field_tp.get_type() == TypeLong
        || field_tp.get_type() == TypeLonglong
        || field_tp.get_type() == TypeNewDecimal)
        && mysql::has_unsigned_flag(field_tp.get_flag())
    {
        tp.add_flag(mysql::UnsignedFlag);
    }
    // TODO: when argument type is timestamp, add not null flag.
}

/// The CEIL / CEILING / FLOOR function classes.
#[derive(Debug)]
pub struct CeilFloorFunctionClass {
    /// The embedded baseFunctionClass.
    pub base: BaseFunctionClass,
    ceil: bool,
}

macro_rules! ceil_floor_class {
    ($name:ident, $fname:expr, $ceil:expr) => {
        #[doc = concat!("The function class of `", stringify!($fname), "`.")]
        pub static $name: CeilFloorFunctionClass = CeilFloorFunctionClass {
            base: BaseFunctionClass {
                func_name: $fname,
                min_args: 1,
                max_args: 1,
            },
            ceil: $ceil,
        };
    };
}

ceil_floor_class!(CEIL_CLASS, ast::Ceil, true);
ceil_floor_class!(CEILING_CLASS, ast::Ceiling, true);
ceil_floor_class!(FLOOR_CLASS, ast::Floor, false);

impl FunctionClass for CeilFloorFunctionClass {
    // go: ceilFunctionClass.getFunction L1
    // go: floorFunctionClass.getFunction L1
    fn get_function(
        &self,
        ctx: &dyn BuildContext,
        args: Vec<ExprRef>,
    ) -> Result<BuiltinFunc, Error> {
        self.base.verify_args(&args)?;
        let ev = ctx.get_eval_ctx();
        let arg0 = args[0].clone();
        let (ret_tp, arg_tp) = get_eval_tp4_floor_and_ceil(&*ev, &arg0);
        let mut bf =
            new_base_builtin_func_with_tp(ctx, self.base.func_name, args, ret_tp, &[arg_tp])?;
        // Go reads args[0] after newBaseBuiltinFuncWithTp replaced it with the wrapped one.
        let wrapped = bf.args[0].clone();
        set_flag4_floor_and_ceil(&*ev, &mut bf.tp, &wrapped);
        // ETInt or ETReal is set correctly by newBaseBuiltinFuncWithTp, only need to handle ETDecimal.
        if ret_tp == ETDecimal {
            bf.tp
                .set_flen_under_limit(wrapped.get_type(&*ev).get_flen());
            bf.tp.set_decimal(0);
        }
        let sig: &'static SigDesc = match (arg_tp, ret_tp, self.ceil) {
            (ETInt, ETInt, true) => &builtinCeilIntToIntSig,
            (ETInt, _, true) => &builtinCeilIntToDecSig,
            (ETDecimal, ETInt, true) => &builtinCeilDecToIntSig,
            (ETDecimal, _, true) => &builtinCeilDecToDecSig,
            (_, _, true) => &builtinCeilRealSig,
            (ETInt, ETInt, false) => &builtinFloorIntToIntSig,
            (ETInt, _, false) => &builtinFloorIntToDecSig,
            (ETDecimal, ETInt, false) => &builtinFloorDecToIntSig,
            (ETDecimal, _, false) => &builtinFloorDecToDecSig,
            (_, _, false) => &builtinFloorRealSig,
        };
        Ok(bf.with_sig(sig))
    }
}

// ---------------------------------------------------------------- the signatures

/// evalReal evals a builtinCeilRealSig.
// go: builtinCeilRealSig.evalReal L1
// go: builtinCeilRealSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinCeilRealSig.vecEvalReal L2 (the vector driver of this body, M-EVAL)
// go: builtinCeilRealSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Real)]
pub fn ceil_real(
    _ctx: &dyn EvalContext,
    _f: &impl SigView,
    val: f64,
) -> Result<Option<f64>, Error> {
    Ok(Some(val.ceil()))
}

/// evalReal evals a builtinFloorRealSig.
// go: builtinFloorRealSig.evalReal L1
// go: builtinFloorRealSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinFloorRealSig.vecEvalReal L2 (the vector driver of this body, M-EVAL)
// go: builtinFloorRealSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Real)]
pub fn floor_real(
    _ctx: &dyn EvalContext,
    _f: &impl SigView,
    val: f64,
) -> Result<Option<f64>, Error> {
    Ok(Some(val.floor()))
}

/// evalInt evals a builtinCeilIntToIntSig / builtinFloorIntToIntSig: the argument itself.
// go: builtinCeilIntToIntSig.evalInt L1
// go: builtinCeilIntToIntSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinCeilIntToIntSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
// go: builtinCeilIntToIntSig.vectorized L2 (SigDesc.vectorized)
// go: builtinFloorIntToIntSig.evalInt L1
// go: builtinFloorIntToIntSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinFloorIntToIntSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
// go: builtinFloorIntToIntSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Int)]
pub fn ceil_floor_int_to_int(
    _ctx: &dyn EvalContext,
    _f: &impl SigView,
    val: i64,
) -> Result<Option<i64>, Error> {
    Ok(Some(val))
}

/// evalDecimal evals a builtinCeilIntToDecSig / builtinFloorIntToDecSig.
// go: builtinCeilIntToDecSig.evalDecimal L1
// go: builtinCeilIntToDecSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinCeilIntToDecSig.vecEvalDecimal L2 (the vector driver of this body, M-EVAL)
// go: builtinCeilIntToDecSig.vectorized L2 (SigDesc.vectorized)
// go: builtinFloorIntToDecSig.evalDecimal L1
// go: builtinFloorIntToDecSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinFloorIntToDecSig.vecEvalDecimal L2 (the vector driver of this body, M-EVAL)
// go: builtinFloorIntToDecSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Decimal)]
pub fn ceil_floor_int_to_dec(
    _ctx: &dyn EvalContext,
    f: &impl SigView,
    val: i64,
) -> Result<Option<MyDecimal>, Error> {
    if mysql::has_unsigned_flag(f.arg_type(0).get_flag()) || val >= 0 {
        return Ok(Some(new_dec_from_uint(val as u64)));
    }
    Ok(Some(new_dec_from_int(val)))
}

/// evalInt evals a builtinCeilDecToIntSig.
// go: builtinCeilDecToIntSig.evalInt L1
// go: builtinCeilDecToIntSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinCeilDecToIntSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
// go: builtinCeilDecToIntSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Int)]
pub fn ceil_dec_to_int(
    _ctx: &dyn EvalContext,
    _f: &impl SigView,
    val: &MyDecimal,
) -> Result<Option<i64>, Error> {
    // err here will only be ErrOverFlow(will never happen) or ErrTruncate(can be ignored).
    let (mut res, err) = val.to_int();
    match err {
        Err(e) if ErrTruncated.equal(&e) => {
            if !val.is_negative() {
                res = res.wrapping_add(1);
            }
        }
        other => other?,
    }
    Ok(Some(res))
}

/// evalInt evals a builtinFloorDecToIntSig.
// go: builtinFloorDecToIntSig.evalInt L1
// go: builtinFloorDecToIntSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinFloorDecToIntSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
// go: builtinFloorDecToIntSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Int)]
pub fn floor_dec_to_int(
    _ctx: &dyn EvalContext,
    _f: &impl SigView,
    val: &MyDecimal,
) -> Result<Option<i64>, Error> {
    // err here will only be ErrOverFlow(will never happen) or ErrTruncate(can be ignored).
    let (mut res, err) = val.to_int();
    match err {
        Err(e) if ErrTruncated.equal(&e) => {
            if val.is_negative() {
                res = res.wrapping_sub(1);
            }
        }
        other => other?,
    }
    Ok(Some(res))
}

/// evalDecimal evals a builtinCeilDecToDecSig.
// go: builtinCeilDecToDecSig.evalDecimal L1
// go: builtinCeilDecToDecSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinCeilDecToDecSig.vecEvalDecimal L2 (the vector driver of this body, M-EVAL)
// go: builtinCeilDecToDecSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Decimal)]
pub fn ceil_dec_to_dec(
    _ctx: &dyn EvalContext,
    _f: &impl SigView,
    val: &MyDecimal,
) -> Result<Option<MyDecimal>, Error> {
    let mut res = MyDecimal::default();
    if val.is_negative() {
        val.round(&mut res, 0, MODE_TRUNCATE)?;
        return Ok(Some(res));
    }

    val.round(&mut res, 0, MODE_TRUNCATE)?;
    if res.compare(val) == 0 {
        return Ok(Some(res));
    }

    let r = res;
    decimal_add(&r, &new_dec_from_int(1), &mut res)?;
    Ok(Some(res))
}

/// evalDecimal evals a builtinFloorDecToDecSig.
// go: builtinFloorDecToDecSig.evalDecimal L1
// go: builtinFloorDecToDecSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinFloorDecToDecSig.vecEvalDecimal L2 (the vector driver of this body, M-EVAL)
// go: builtinFloorDecToDecSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Decimal)]
pub fn floor_dec_to_dec(
    _ctx: &dyn EvalContext,
    _f: &impl SigView,
    val: &MyDecimal,
) -> Result<Option<MyDecimal>, Error> {
    let mut res = MyDecimal::default();
    if !val.is_negative() {
        val.round(&mut res, 0, MODE_TRUNCATE)?;
        return Ok(Some(res));
    }

    val.round(&mut res, 0, MODE_TRUNCATE)?;
    if res.compare(val) == 0 {
        return Ok(Some(res));
    }

    let r = res;
    decimal_sub(&r, &new_dec_from_int(1), &mut res)?;
    Ok(Some(res))
}

// ---------------------------------------------------------------- signature descriptors

macro_rules! math_sig {
    ($name:ident, $pb:expr, $drivers:ident) => {
        #[doc = concat!("`", stringify!($name), "`.")]
        pub static $name: SigDesc = SigDesc {
            name: stringify!($name),
            pb_code: $pb,
            vectorized: true,
            drivers: $drivers,
            args_fallback: crate::eval::ArgsFallback::None,
        };
    };
}

// tipb.ScalarFuncSig codes (go-tipb expression.pb.go).
math_sig!(builtinCeilIntToDecSig, 2105, CEIL_FLOOR_INT_TO_DEC);
math_sig!(builtinCeilIntToIntSig, 2106, CEIL_FLOOR_INT_TO_INT);
math_sig!(builtinCeilDecToIntSig, 2107, CEIL_DEC_TO_INT);
math_sig!(builtinCeilDecToDecSig, 2108, CEIL_DEC_TO_DEC);
math_sig!(builtinCeilRealSig, 2109, CEIL_REAL);
math_sig!(builtinFloorIntToDecSig, 2110, CEIL_FLOOR_INT_TO_DEC);
math_sig!(builtinFloorIntToIntSig, 2111, CEIL_FLOOR_INT_TO_INT);
math_sig!(builtinFloorDecToIntSig, 2112, FLOOR_DEC_TO_INT);
math_sig!(builtinFloorDecToDecSig, 2113, FLOOR_DEC_TO_DEC);
math_sig!(builtinFloorRealSig, 2114, FLOOR_REAL);

// ================================================================ batch 4

/// `types.ErrOverflow.GenWithStackByArgs(tp, s)`.
// glue: the overflow error of the math signatures
fn overflow(tp: &'static str, s: String) -> Error {
    ErrOverflow.gen_with_stack_by_args(vec![Arg::from(tp), Arg::from(s)])
}

/// `strconv.FormatFloat(f, 'f', -1, 64)`.
// glue: strconv.FormatFloat(f, 'f', -1, 64) of the math error messages
fn fmt_f(f: f64) -> String {
    format_float(f, b'f', -1, 64)
}

/// `tc.AppendWarning(ErrInvalidArgumentForLogarithm)`.
// glue: the logarithm warning
fn log_warning(ctx: &dyn EvalContext) {
    type_ctx(ctx).append_warning((*ErrInvalidArgumentForLogarithm).clone());
}

/// A function class `{baseFunctionClass{name, min, max}}` whose `getFunction` is `get`.
pub struct MathFunctionClass {
    /// The embedded baseFunctionClass.
    pub base: BaseFunctionClass,
    get: fn(&BaseFunctionClass, &dyn BuildContext, Vec<ExprRef>) -> Result<BuiltinFunc, Error>,
}

impl std::fmt::Debug for MathFunctionClass {
    // glue: trait impl
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MathFunctionClass")
            .field("base", &self.base)
            .finish()
    }
}

impl FunctionClass for MathFunctionClass {
    // glue: dispatch to the class's getFunction (each one is annotated where it is ported)
    fn get_function(
        &self,
        ctx: &dyn BuildContext,
        args: Vec<ExprRef>,
    ) -> Result<BuiltinFunc, Error> {
        (self.get)(&self.base, ctx, args)
    }
}

macro_rules! math_class {
    ($name:ident, $fname:expr, $min:expr, $max:expr, $get:ident) => {
        #[doc = concat!("The function class of `", stringify!($fname), "`.")]
        pub static $name: MathFunctionClass = MathFunctionClass {
            base: BaseFunctionClass {
                func_name: $fname,
                min_args: $min,
                max_args: $max,
            },
            get: $get,
        };
    };
}

math_class!(ABS_CLASS, ast::Abs, 1, 1, abs_get_function);
math_class!(ROUND_CLASS, ast::Round, 1, 2, round_get_function);
math_class!(TRUNCATE_CLASS, ast::Truncate, 2, 2, truncate_get_function);
math_class!(LN_CLASS, ast::Ln, 1, 1, log_get_function);
math_class!(LOG_CLASS, ast::Log, 1, 2, log_get_function);
math_class!(LOG2_CLASS, ast::Log2, 1, 1, log2_get_function);
math_class!(LOG10_CLASS, ast::Log10, 1, 1, log10_get_function);
math_class!(RAND_CLASS, ast::Rand, 0, 1, rand_get_function);
math_class!(POW_CLASS, ast::Pow, 2, 2, pow_get_function);
math_class!(POWER_CLASS, ast::Power, 2, 2, pow_get_function);
math_class!(SIGN_CLASS, ast::Sign, 1, 1, sign_get_function);
math_class!(SQRT_CLASS, ast::Sqrt, 1, 1, sqrt_get_function);
math_class!(ACOS_CLASS, ast::Acos, 1, 1, acos_get_function);
math_class!(ASIN_CLASS, ast::Asin, 1, 1, asin_get_function);
math_class!(ATAN_CLASS, ast::Atan, 1, 2, atan_get_function);
math_class!(ATAN2_CLASS, ast::Atan2, 2, 2, atan_get_function);
math_class!(COS_CLASS, ast::Cos, 1, 1, cos_get_function);
math_class!(COT_CLASS, ast::Cot, 1, 1, cot_get_function);
math_class!(DEGREES_CLASS, ast::Degrees, 1, 1, degrees_get_function);
math_class!(EXP_CLASS, ast::Exp, 1, 1, exp_get_function);
math_class!(CONV_CLASS, ast::Conv, 3, 3, conv_get_function);
math_class!(CRC32_CLASS, ast::CRC32, 1, 1, crc32_get_function);
math_class!(PI_CLASS, ast::PI, 0, 0, pi_get_function);
math_class!(RADIANS_CLASS, ast::Radians, 1, 1, radians_get_function);
math_class!(SIN_CLASS, ast::Sin, 1, 1, sin_get_function);
math_class!(TAN_CLASS, ast::Tan, 1, 1, tan_get_function);

/// A class whose `getFunction` is `newBaseBuiltinFuncWithTp(ETReal, ETReal...)` and one
/// signature (SQRT, ACOS, ASIN, COS, COT, DEGREES, EXP, RADIANS, SIN, TAN, LOG2, LOG10,
/// POW).
// glue: the shared shape of the one-signature real getFunction methods
fn real_fn(
    c: &BaseFunctionClass,
    ctx: &dyn BuildContext,
    args: Vec<ExprRef>,
    n: usize,
    sig: &'static SigDesc,
) -> Result<BuiltinFunc, Error> {
    c.verify_args(&args)?;
    let arg_tps = vec![ETReal; n];
    let bf = new_base_builtin_func_with_tp(ctx, c.func_name, args, ETReal, &arg_tps)?;
    Ok(bf.with_sig(sig))
}

// ---------------------------------------------------------------- ABS

// go: absFunctionClass.getFunction L1
fn abs_get_function(
    c: &BaseFunctionClass,
    ctx: &dyn BuildContext,
    args: Vec<ExprRef>,
) -> Result<BuiltinFunc, Error> {
    c.verify_args(&args)?;
    let ev = ctx.get_eval_ctx();
    // Go takes the type pointer before newBaseBuiltinFuncWithTp (no PropagateType of a
    // DECIMAL argument: ABS never wraps it as REAL).
    let arg_field_tp = args[0].get_type(&*ev).into_owned();
    let mut arg_tp = arg_field_tp.eval_type();
    if arg_tp != ETInt && arg_tp != ETDecimal {
        arg_tp = ETReal;
    }
    let mut bf = new_base_builtin_func_with_tp(ctx, c.func_name, args, arg_tp, &[arg_tp])?;
    if mysql::has_unsigned_flag(arg_field_tp.get_flag()) {
        bf.tp.add_flag(mysql::UnsignedFlag);
    }
    if arg_tp == ETReal {
        let (flen, decimal) = mysql::get_default_field_length_and_decimal(TypeDouble);
        bf.tp.set_flen(flen);
        bf.tp.set_decimal(decimal);
    } else {
        bf.tp.set_flen_under_limit(arg_field_tp.get_flen());
        bf.tp.set_decimal_under_limit(arg_field_tp.get_decimal());
    }
    let sig: &'static SigDesc = match arg_tp {
        ETInt => {
            if mysql::has_unsigned_flag(arg_field_tp.get_flag()) {
                &builtinAbsUIntSig
            } else {
                &builtinAbsIntSig
            }
        }
        ETDecimal => &builtinAbsDecSig,
        _ => &builtinAbsRealSig,
    };
    Ok(bf.with_sig(sig))
}

/// evalReal evals ABS(value).
// go: builtinAbsRealSig.evalReal L1
// go: builtinAbsRealSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinAbsRealSig.vecEvalReal L2 (the vector driver of this body, M-EVAL)
// go: builtinAbsRealSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Real)]
pub fn abs_real(_ctx: &dyn EvalContext, _f: &impl SigView, val: f64) -> Result<Option<f64>, Error> {
    Ok(Some(gomath::abs(val)))
}

/// evalInt evals ABS(value).
// go: builtinAbsIntSig.evalInt L1
// go: builtinAbsIntSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinAbsIntSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
// go: builtinAbsIntSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Int)]
pub fn abs_int(_ctx: &dyn EvalContext, _f: &impl SigView, val: i64) -> Result<Option<i64>, Error> {
    if val >= 0 {
        return Ok(Some(val));
    }
    if val == i64::MIN {
        return Err(overflow("BIGINT", format!("abs({val})")));
    }
    Ok(Some(-val))
}

/// evalInt evals ABS(value) of an unsigned argument: the argument itself.
// go: builtinAbsUIntSig.evalInt L1
// go: builtinAbsUIntSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinAbsUIntSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
// go: builtinAbsUIntSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Int)]
pub fn abs_uint(_ctx: &dyn EvalContext, _f: &impl SigView, val: i64) -> Result<Option<i64>, Error> {
    Ok(Some(val))
}

/// evalDecimal evals ABS(value).
// go: builtinAbsDecSig.evalDecimal L1
// go: builtinAbsDecSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinAbsDecSig.vecEvalDecimal L2 (the vector driver of this body, M-EVAL)
// go: builtinAbsDecSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Decimal)]
pub fn abs_dec(
    _ctx: &dyn EvalContext,
    _f: &impl SigView,
    val: &MyDecimal,
) -> Result<Option<MyDecimal>, Error> {
    let mut to = MyDecimal::default();
    if !val.is_negative() {
        to = *val;
    } else {
        decimal_sub(&MyDecimal::default(), val, &mut to)?;
    }
    Ok(Some(to))
}

// ---------------------------------------------------------------- ROUND / TRUNCATE

// go: roundFunctionClass.getFunction L1
fn round_get_function(
    c: &BaseFunctionClass,
    ctx: &dyn BuildContext,
    args: Vec<ExprRef>,
) -> Result<BuiltinFunc, Error> {
    c.verify_args(&args)?;
    let ev = ctx.get_eval_ctx();
    let mut arg_tp = args[0].get_type(&*ev).eval_type();
    if arg_tp != ETInt && arg_tp != ETDecimal {
        arg_tp = ETReal;
    }
    let mut arg_tps = vec![arg_tp];
    if args.len() > 1 {
        arg_tps.push(ETInt);
    }
    let mut bf = new_base_builtin_func_with_tp(ctx, c.func_name, args, arg_tp, &arg_tps)?;
    // Go reads args[0] after newBaseBuiltinFuncWithTp replaced it with the wrapped one.
    let arg_field_tp = bf.args[0].get_type(&*ev).into_owned();
    if mysql::has_unsigned_flag(arg_field_tp.get_flag()) {
        bf.tp.add_flag(mysql::UnsignedFlag);
    }
    // ETInt or ETReal is set correctly by newBaseBuiltinFuncWithTp, only need to handle ETDecimal.
    if arg_tp == ETDecimal {
        bf.tp.set_flen_under_limit(arg_field_tp.get_flen());
        let dec = calculate_decimal4_round_and_truncate(ctx, &bf.args, arg_tp);
        bf.tp.set_decimal_under_limit(dec);
        if bf.tp.get_decimal() != UnspecifiedLength {
            if arg_field_tp.get_decimal() != UnspecifiedLength {
                let decimal_delta = bf.tp.get_decimal() - arg_field_tp.get_decimal();
                bf.tp
                    .set_flen_under_limit(bf.tp.get_flen() + decimal_delta.max(0));
            } else {
                bf.tp
                    .set_flen_under_limit(arg_field_tp.get_flen() + bf.tp.get_decimal());
            }
        }
    }
    let sig: &'static SigDesc = match (bf.args.len() > 1, arg_tp) {
        (true, ETInt) => &builtinRoundWithFracIntSig,
        (true, ETDecimal) => &builtinRoundWithFracDecSig,
        (true, _) => &builtinRoundWithFracRealSig,
        (false, ETInt) => &builtinRoundIntSig,
        (false, ETDecimal) => &builtinRoundDecSig,
        (false, _) => &builtinRoundRealSig,
    };
    Ok(bf.with_sig(sig))
}

/// calculateDecimal4RoundAndTruncate calculates tp.decimals of round/truncate func.
// go: calculateDecimal4RoundAndTruncate L1
pub fn calculate_decimal4_round_and_truncate(
    ctx: &dyn BuildContext,
    args: &[ExprRef],
    ret_type: EvalType,
) -> isize {
    if ret_type == ETInt || args.len() <= 1 {
        return 0;
    }
    let ev = ctx.get_eval_ctx();
    let Some(second_const) = args[1].as_constant() else {
        return args[0].get_type(&*ev).get_decimal();
    };
    let arg_dec = match second_const.eval_int(&*ev, &EvalState::new(), Row::default()) {
        Ok(Some(d)) if d >= 0 => d,
        _ => return 0,
    };
    if arg_dec > MaxDecimalScale as i64 {
        return MaxDecimalScale;
    }
    arg_dec as isize
}

/// evalReal evals ROUND(value).
// go: builtinRoundRealSig.evalReal L1
// go: builtinRoundRealSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinRoundRealSig.vecEvalReal L2 (the vector driver of this body, M-EVAL)
// go: builtinRoundRealSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Real)]
pub fn round_real(
    _ctx: &dyn EvalContext,
    _f: &impl SigView,
    val: f64,
) -> Result<Option<f64>, Error> {
    Ok(Some(helper::round(val, 0)))
}

/// evalInt evals ROUND(value): the argument itself.
// go: builtinRoundIntSig.evalInt L1
// go: builtinRoundIntSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinRoundIntSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
// go: builtinRoundIntSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Int)]
pub fn round_int(
    _ctx: &dyn EvalContext,
    _f: &impl SigView,
    val: i64,
) -> Result<Option<i64>, Error> {
    Ok(Some(val))
}

/// evalDecimal evals ROUND(value).
// go: builtinRoundDecSig.evalDecimal L1
// go: builtinRoundDecSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinRoundDecSig.vecEvalDecimal L2 (the vector driver of this body, M-EVAL)
// go: builtinRoundDecSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Decimal)]
pub fn round_dec(
    _ctx: &dyn EvalContext,
    _f: &impl SigView,
    val: &MyDecimal,
) -> Result<Option<MyDecimal>, Error> {
    let mut to = MyDecimal::default();
    val.round(&mut to, 0, MODE_HALF_UP)?;
    Ok(Some(to))
}

/// evalReal evals ROUND(value, frac).
// go: builtinRoundWithFracRealSig.evalReal L1
// go: builtinRoundWithFracRealSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinRoundWithFracRealSig.vecEvalReal L2 (the vector driver of this body, M-EVAL)
// go: builtinRoundWithFracRealSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Real)]
pub fn round_with_frac_real(
    _ctx: &dyn EvalContext,
    _f: &impl SigView,
    val: f64,
    frac: i64,
) -> Result<Option<f64>, Error> {
    Ok(Some(helper::round(val, frac as isize)))
}

/// evalInt evals ROUND(value, frac).
// go: builtinRoundWithFracIntSig.evalInt L1
// go: builtinRoundWithFracIntSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinRoundWithFracIntSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
// go: builtinRoundWithFracIntSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Int)]
pub fn round_with_frac_int(
    _ctx: &dyn EvalContext,
    _f: &impl SigView,
    val: i64,
    frac: i64,
) -> Result<Option<i64>, Error> {
    Ok(Some(f64_to_i64(helper::round(val as f64, frac as isize)))) // [M-F2I]
}

/// evalDecimal evals ROUND(value, frac).
// go: builtinRoundWithFracDecSig.evalDecimal L1
// go: builtinRoundWithFracDecSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinRoundWithFracDecSig.vecEvalDecimal L2 (the vector driver of this body, M-EVAL)
// go: builtinRoundWithFracDecSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Decimal)]
pub fn round_with_frac_dec(
    _ctx: &dyn EvalContext,
    f: &impl SigView,
    val: &MyDecimal,
    frac: i64,
) -> Result<Option<MyDecimal>, Error> {
    let mut to = MyDecimal::default();
    val.round(
        &mut to,
        (frac as isize).min(f.tp.get_decimal()),
        MODE_HALF_UP,
    )?;
    Ok(Some(to))
}

// go: truncateFunctionClass.getFunction L1
fn truncate_get_function(
    c: &BaseFunctionClass,
    ctx: &dyn BuildContext,
    args: Vec<ExprRef>,
) -> Result<BuiltinFunc, Error> {
    c.verify_args(&args)?;
    let ev = ctx.get_eval_ctx();
    let mut arg_tp = args[0].get_type(&*ev).eval_type();
    if arg_tp.is_string_kind() {
        arg_tp = ETReal;
    }
    let mut bf = new_base_builtin_func_with_tp(ctx, c.func_name, args, arg_tp, &[arg_tp, ETInt])?;
    // Go reads args[0] after newBaseBuiltinFuncWithTp replaced it with the wrapped one.
    let arg_field_tp = bf.args[0].get_type(&*ev).into_owned();
    // ETInt or ETReal is set correctly by newBaseBuiltinFuncWithTp, only need to handle ETDecimal.
    if arg_tp == ETDecimal {
        let dec = calculate_decimal4_round_and_truncate(ctx, &bf.args, arg_tp);
        bf.tp.set_decimal_under_limit(dec);
        bf.tp.set_flen_under_limit(
            arg_field_tp.get_flen() - arg_field_tp.get_decimal() + bf.tp.get_decimal(),
        );
    }
    if mysql::has_unsigned_flag(arg_field_tp.get_flag()) {
        bf.tp.add_flag(mysql::UnsignedFlag);
    }
    let sig: &'static SigDesc = match arg_tp {
        ETInt => {
            if mysql::has_unsigned_flag(arg_field_tp.get_flag()) {
                &builtinTruncateUintSig
            } else {
                &builtinTruncateIntSig
            }
        }
        ETReal => &builtinTruncateRealSig,
        ETDecimal => &builtinTruncateDecimalSig,
        // Go: sig stays nil and getFunction returns (nil, nil) (TIME / DURATION / JSON /
        // VECTOR arguments); NewFunction then dereferences the nil signature.
        _ => panic!("truncate: no signature for the argument type (Go returns a nil builtinFunc)"),
    };
    Ok(bf.with_sig(sig))
}

/// evalDecimal evals a TRUNCATE(X,D).
// go: builtinTruncateDecimalSig.evalDecimal L1
// go: builtinTruncateDecimalSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinTruncateDecimalSig.vecEvalDecimal L2 (the vector driver of this body, M-EVAL)
// go: builtinTruncateDecimalSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Decimal)]
pub fn truncate_decimal(
    _ctx: &dyn EvalContext,
    f: &impl SigView,
    x: &MyDecimal,
    d: i64,
) -> Result<Option<MyDecimal>, Error> {
    let mut result = MyDecimal::default();
    x.round(
        &mut result,
        (d as isize).min(f.tp.get_decimal()),
        MODE_TRUNCATE,
    )?;
    Ok(Some(result))
}

/// evalReal evals a TRUNCATE(X,D).
// go: builtinTruncateRealSig.evalReal L1
// go: builtinTruncateRealSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinTruncateRealSig.vecEvalReal L2 (the vector driver of this body, M-EVAL)
// go: builtinTruncateRealSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Real)]
pub fn truncate_real(
    _ctx: &dyn EvalContext,
    _f: &impl SigView,
    x: f64,
    d: i64,
) -> Result<Option<f64>, Error> {
    Ok(Some(helper::truncate(x, d as isize)))
}

/// evalInt evals a TRUNCATE(X,D).
// go: builtinTruncateIntSig.evalInt L1
// go: builtinTruncateIntSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinTruncateIntSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
// go: builtinTruncateIntSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Int)]
pub fn truncate_int(
    _ctx: &dyn EvalContext,
    f: &impl SigView,
    x: i64,
    d: i64,
) -> Result<Option<i64>, Error> {
    if mysql::has_unsigned_flag(f.arg_type(1).get_flag()) {
        return Ok(Some(x));
    }
    if d >= 0 {
        return Ok(Some(x));
    }
    // -MinInt = MinInt, special case
    if d == i64::MIN {
        return Ok(Some(0));
    }
    let shift = f64_to_i64(gomath::pow10(-d as isize)); // [M-F2I]
    Ok(Some(x.wrapping_div(shift).wrapping_mul(shift)))
}

/// evalInt evals a TRUNCATE(X,D) of an unsigned X. The row version returns X before it
/// evaluates D when D is unsigned (so a NULL D gives X); the vector version evaluates both
/// columns and merges their NULLs first (D08.5 spec §3.2).
// go: builtinTruncateUintSig.evalInt L1
// go: builtinTruncateUintSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinTruncateUintSig.vecEvalInt L2 (the vector driver of this body: both columns, NULLs merged, as in Go)
// go: builtinTruncateUintSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Int, lazy, args(Int, Int))]
pub fn truncate_uint(
    ctx: &dyn EvalContext,
    f: &impl SigView,
    args: &mut impl Args,
) -> Result<Option<i64>, Error> {
    let Some(x) = args.int(ctx, 0)? else {
        return Ok(None);
    };
    let unsigned_d = mysql::has_unsigned_flag(f.arg_type(1).get_flag());
    if unsigned_d && !f.vector() {
        return Ok(Some(x));
    }
    let uintx = x as u64;
    let Some(d) = args.int(ctx, 1)? else {
        return Ok(None);
    };
    if unsigned_d || d >= 0 {
        return Ok(Some(x));
    }
    // -MinInt = MinInt, special case
    if d == i64::MIN {
        return Ok(Some(0));
    }
    let shift = f64_to_u64(gomath::pow10(-d as isize)); // [M-F2I]
    Ok(Some((uintx / shift * shift) as i64))
}

// ---------------------------------------------------------------- LOG

// go: logFunctionClass.getFunction L1
fn log_get_function(
    c: &BaseFunctionClass,
    ctx: &dyn BuildContext,
    args: Vec<ExprRef>,
) -> Result<BuiltinFunc, Error> {
    if args.len() == 1 {
        real_fn(c, ctx, args, 1, &builtinLog1ArgSig)
    } else {
        real_fn(c, ctx, args, 2, &builtinLog2ArgsSig)
    }
}

/// evalReal evals a builtinLog1ArgSig, corresponding to log(x).
// go: builtinLog1ArgSig.evalReal L1
// go: builtinLog1ArgSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinLog1ArgSig.vecEvalReal L2 (the vector driver of this body, M-EVAL)
// go: builtinLog1ArgSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Real)]
pub fn log1_arg(ctx: &dyn EvalContext, _f: &impl SigView, val: f64) -> Result<Option<f64>, Error> {
    if val <= 0.0 {
        log_warning(ctx);
        return Ok(None);
    }
    Ok(Some(gomath::log(val)))
}

/// evalReal evals a builtinLog2ArgsSig, corresponding to log(b, x).
// go: builtinLog2ArgsSig.evalReal L1
// go: builtinLog2ArgsSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinLog2ArgsSig.vecEvalReal L2 (the vector driver of this body, M-EVAL)
// go: builtinLog2ArgsSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Real)]
pub fn log2_args(
    ctx: &dyn EvalContext,
    _f: &impl SigView,
    val1: f64,
    val2: f64,
) -> Result<Option<f64>, Error> {
    if val1 <= 0.0 || val1 == 1.0 || val2 <= 0.0 {
        log_warning(ctx);
        return Ok(None);
    }
    Ok(Some(gomath::log(val2) / gomath::log(val1)))
}

// go: log2FunctionClass.getFunction L1
fn log2_get_function(
    c: &BaseFunctionClass,
    ctx: &dyn BuildContext,
    args: Vec<ExprRef>,
) -> Result<BuiltinFunc, Error> {
    real_fn(c, ctx, args, 1, &builtinLog2Sig)
}

/// evalReal evals a builtinLog2Sig.
// go: builtinLog2Sig.evalReal L1
// go: builtinLog2Sig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinLog2Sig.vecEvalReal L2 (the vector driver of this body, M-EVAL)
// go: builtinLog2Sig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Real)]
pub fn log2(ctx: &dyn EvalContext, _f: &impl SigView, val: f64) -> Result<Option<f64>, Error> {
    if val <= 0.0 {
        log_warning(ctx);
        return Ok(None);
    }
    Ok(Some(gomath::log2(val)))
}

// go: log10FunctionClass.getFunction L1
fn log10_get_function(
    c: &BaseFunctionClass,
    ctx: &dyn BuildContext,
    args: Vec<ExprRef>,
) -> Result<BuiltinFunc, Error> {
    real_fn(c, ctx, args, 1, &builtinLog10Sig)
}

/// evalReal evals a builtinLog10Sig.
// go: builtinLog10Sig.evalReal L1
// go: builtinLog10Sig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinLog10Sig.vecEvalReal L2 (the vector driver of this body, M-EVAL)
// go: builtinLog10Sig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Real)]
pub fn log10(ctx: &dyn EvalContext, _f: &impl SigView, val: f64) -> Result<Option<f64>, Error> {
    if val <= 0.0 {
        log_warning(ctx);
        return Ok(None);
    }
    Ok(Some(gomath::log10(val)))
}

// ---------------------------------------------------------------- RAND

// go: randFunctionClass.getFunction L1
fn rand_get_function(
    c: &BaseFunctionClass,
    ctx: &dyn BuildContext,
    args: Vec<ExprRef>,
) -> Result<BuiltinFunc, Error> {
    c.verify_args(&args)?;
    let arg_tps: &[EvalType] = if args.is_empty() { &[] } else { &[ETInt] };
    let bf = new_base_builtin_func_with_tp(ctx, c.func_name, args, ETReal, arg_tps)?;
    if bf.args.is_empty() {
        let rng = ctx.rng();
        let mut sig = bf.with_sig(&builtinRandSig);
        sig.extra = SigExtra::Rand { rng };
        return Ok(sig);
    }
    // Go tests args[0] after newBaseBuiltinFuncWithTp replaced it with the wrapped one.
    if let Some(con) = bf.args[0].as_constant() {
        // According to MySQL manual: With a constant initializer argument, the seed is
        // initialized once when the statement is prepared, prior to execution.
        let ev = ctx.get_eval_ctx();
        // When the seed is null we need to use 0 as the seed. The behavior same as MySQL.
        let seed = con
            .eval_int(&*ev, &EvalState::new(), Row::default())?
            .unwrap_or(0);
        let rng = Arc::new(parking_lot::Mutex::new(MysqlRng::new_with_seed(seed)));
        let mut sig = bf.with_sig(&builtinRandSig);
        sig.extra = SigExtra::Rand { rng };
        return Ok(sig);
    }
    Ok(bf.with_sig(&builtinRandWithSeedFirstGenSig))
}

/// evalReal evals RAND(): the next value of the signature's generator (the session's
/// `Rng()` for `RAND()`, a generator seeded once for `RAND(constant)`). Go's `Clone` shares
/// the `*MysqlRng` (`&builtinRandSig{mysqlRng: b.mysqlRng}`), so does the Rust clone of the
/// `Arc` in [`SigExtra::Rand`] (D08.1 spec §3.5).
// go: builtinRandSig.evalReal L1
// go: builtinRandSig.Clone L2 (BuiltinFunc::clone_from_base clones SigExtra::Rand: the generator is shared, as in Go)
// go: builtinRandSig.vecEvalReal L2 (the vector driver of this body: one Gen per row, in row order, as in Go)
// go: builtinRandSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Real, lazy)]
pub fn rand(
    _ctx: &dyn EvalContext,
    f: &impl SigView,
    _args: &mut impl Args,
) -> Result<Option<f64>, Error> {
    let SigExtra::Rand { rng } = &f.extra else {
        panic!("builtinRandSig without a generator");
    };
    let res = rng.lock().r#gen();
    Ok(Some(res))
}

/// evalReal evals RAND(N) of a non-constant N: a generator seeded with the value of each
/// row (0 when NULL).
// go: builtinRandWithSeedFirstGenSig.evalReal L1
// go: builtinRandWithSeedFirstGenSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinRandWithSeedFirstGenSig.vecEvalReal L2 (the vector driver of this body, M-EVAL)
// go: builtinRandWithSeedFirstGenSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Real, lazy, args(Int))]
pub fn rand_with_seed_first_gen(
    ctx: &dyn EvalContext,
    _f: &impl SigView,
    args: &mut impl Args,
) -> Result<Option<f64>, Error> {
    let seed = args.int(ctx, 0)?;
    // b.args[0] is promised to be a non-constant(such as a column name) in
    // builtinRandWithSeedFirstGenSig, the seed is initialized with the value for each
    // invocation of RAND().
    let mut rng = MysqlRng::new_with_seed(seed.unwrap_or(0));
    Ok(Some(rng.r#gen()))
}

// ---------------------------------------------------------------- POW, SIGN, SQRT

// go: powFunctionClass.getFunction L1
fn pow_get_function(
    c: &BaseFunctionClass,
    ctx: &dyn BuildContext,
    args: Vec<ExprRef>,
) -> Result<BuiltinFunc, Error> {
    real_fn(c, ctx, args, 2, &builtinPowSig)
}

/// evalReal evals POW(x, y).
// go: builtinPowSig.evalReal L1
// go: builtinPowSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinPowSig.vecEvalReal L2 (the vector driver of this body, M-EVAL)
// go: builtinPowSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Real)]
pub fn pow(
    _ctx: &dyn EvalContext,
    _f: &impl SigView,
    x: f64,
    y: f64,
) -> Result<Option<f64>, Error> {
    let power = gomath::pow(x, y);
    if power.is_infinite() || power.is_nan() {
        return Err(overflow(
            "DOUBLE",
            format!("pow({}, {})", fmt_f(x), fmt_f(y)),
        ));
    }
    Ok(Some(power))
}

// go: signFunctionClass.getFunction L1
fn sign_get_function(
    c: &BaseFunctionClass,
    ctx: &dyn BuildContext,
    args: Vec<ExprRef>,
) -> Result<BuiltinFunc, Error> {
    c.verify_args(&args)?;
    let bf = new_base_builtin_func_with_tp(ctx, c.func_name, args, ETInt, &[ETReal])?;
    Ok(bf.with_sig(&builtinSignSig))
}

/// evalInt evals SIGN(v). For NaN the row version gives -1 (not > 0, not == 0), the vector
/// version 0 (not > 0, not < 0); each driver follows its own Go version (D08.5 spec §3.2).
// go: builtinSignSig.evalInt L1
// go: builtinSignSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinSignSig.vecEvalInt L2 (the vector driver of this body: NaN gives 0, as in Go)
// go: builtinSignSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Int)]
pub fn sign(_ctx: &dyn EvalContext, f: &impl SigView, val: f64) -> Result<Option<i64>, Error> {
    if val > 0.0 {
        return Ok(Some(1));
    }
    if f.vector() {
        if val < 0.0 {
            return Ok(Some(-1));
        }
        return Ok(Some(0));
    }
    if val == 0.0 {
        return Ok(Some(0));
    }
    Ok(Some(-1))
}

// go: sqrtFunctionClass.getFunction L1
fn sqrt_get_function(
    c: &BaseFunctionClass,
    ctx: &dyn BuildContext,
    args: Vec<ExprRef>,
) -> Result<BuiltinFunc, Error> {
    real_fn(c, ctx, args, 1, &builtinSqrtSig)
}

/// evalReal evals a SQRT(x) (NULL for x < 0).
// go: builtinSqrtSig.evalReal L1
// go: builtinSqrtSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinSqrtSig.vecEvalReal L2 (the vector driver of this body, M-EVAL)
// go: builtinSqrtSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Real)]
pub fn sqrt(_ctx: &dyn EvalContext, _f: &impl SigView, val: f64) -> Result<Option<f64>, Error> {
    if val < 0.0 {
        return Ok(None);
    }
    Ok(Some(val.sqrt())) // math.Sqrt: the hardware square root on amd64 and arm64
}

// ---------------------------------------------------------------- trigonometry

// go: acosFunctionClass.getFunction L1
fn acos_get_function(
    c: &BaseFunctionClass,
    ctx: &dyn BuildContext,
    args: Vec<ExprRef>,
) -> Result<BuiltinFunc, Error> {
    real_fn(c, ctx, args, 1, &builtinAcosSig)
}

/// evalReal evals a builtinAcosSig (NULL outside [-1, 1]; NaN passes through).
// go: builtinAcosSig.evalReal L1
// go: builtinAcosSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinAcosSig.vecEvalReal L2 (the vector driver of this body, M-EVAL)
// go: builtinAcosSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Real)]
pub fn acos(_ctx: &dyn EvalContext, _f: &impl SigView, val: f64) -> Result<Option<f64>, Error> {
    // Go's `val < -1 || val > 1`: NaN is not NULL (`!contains` would be).
    #[allow(clippy::manual_range_contains)]
    if val < -1.0 || val > 1.0 {
        return Ok(None);
    }
    Ok(Some(gomath::acos(val)))
}

// go: asinFunctionClass.getFunction L1
fn asin_get_function(
    c: &BaseFunctionClass,
    ctx: &dyn BuildContext,
    args: Vec<ExprRef>,
) -> Result<BuiltinFunc, Error> {
    real_fn(c, ctx, args, 1, &builtinAsinSig)
}

/// evalReal evals a builtinAsinSig (NULL outside [-1, 1]; NaN passes through).
// go: builtinAsinSig.evalReal L1
// go: builtinAsinSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinAsinSig.vecEvalReal L2 (the vector driver of this body, M-EVAL)
// go: builtinAsinSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Real)]
pub fn asin(_ctx: &dyn EvalContext, _f: &impl SigView, val: f64) -> Result<Option<f64>, Error> {
    // Go's `val < -1 || val > 1`: NaN is not NULL (`!contains` would be).
    #[allow(clippy::manual_range_contains)]
    if val < -1.0 || val > 1.0 {
        return Ok(None);
    }
    Ok(Some(gomath::asin(val)))
}

// go: atanFunctionClass.getFunction L1
fn atan_get_function(
    c: &BaseFunctionClass,
    ctx: &dyn BuildContext,
    args: Vec<ExprRef>,
) -> Result<BuiltinFunc, Error> {
    c.verify_args(&args)?;
    if args.len() == 1 {
        real_fn(c, ctx, args, 1, &builtinAtan1ArgSig)
    } else {
        real_fn(c, ctx, args, 2, &builtinAtan2ArgsSig)
    }
}

/// evalReal evals a builtinAtan1ArgSig, corresponding to atan(x).
// go: builtinAtan1ArgSig.evalReal L1
// go: builtinAtan1ArgSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinAtan1ArgSig.vecEvalReal L2 (the vector driver of this body, M-EVAL)
// go: builtinAtan1ArgSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Real)]
pub fn atan1_arg(
    _ctx: &dyn EvalContext,
    _f: &impl SigView,
    val: f64,
) -> Result<Option<f64>, Error> {
    Ok(Some(gomath::atan(val)))
}

/// evalReal evals a builtinAtan2ArgsSig, corresponding to atan(y, x).
// go: builtinAtan2ArgsSig.evalReal L1
// go: builtinAtan2ArgsSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinAtan2ArgsSig.vecEvalReal L2 (the vector driver of this body, M-EVAL)
// go: builtinAtan2ArgsSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Real)]
pub fn atan2_args(
    _ctx: &dyn EvalContext,
    _f: &impl SigView,
    val1: f64,
    val2: f64,
) -> Result<Option<f64>, Error> {
    Ok(Some(gomath::atan2(val1, val2)))
}

// go: cosFunctionClass.getFunction L1
fn cos_get_function(
    c: &BaseFunctionClass,
    ctx: &dyn BuildContext,
    args: Vec<ExprRef>,
) -> Result<BuiltinFunc, Error> {
    real_fn(c, ctx, args, 1, &builtinCosSig)
}

/// evalReal evals a builtinCosSig.
// go: builtinCosSig.evalReal L1
// go: builtinCosSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinCosSig.vecEvalReal L2 (the vector driver of this body, M-EVAL)
// go: builtinCosSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Real)]
pub fn cos(_ctx: &dyn EvalContext, _f: &impl SigView, val: f64) -> Result<Option<f64>, Error> {
    Ok(Some(gomath::cos(val)))
}

// go: cotFunctionClass.getFunction L1
fn cot_get_function(
    c: &BaseFunctionClass,
    ctx: &dyn BuildContext,
    args: Vec<ExprRef>,
) -> Result<BuiltinFunc, Error> {
    real_fn(c, ctx, args, 1, &builtinCotSig)
}

/// evalReal evals a builtinCotSig. When `tan != 0` but `1/tan` is infinite or NaN, the row
/// version returns the overflow error and the vector version keeps the argument as the
/// result without an error (D08.5 spec §3.2); each driver follows its own Go version.
// go: builtinCotSig.evalReal L1
// go: builtinCotSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinCotSig.vecEvalReal L2 (the vector driver of this body: tan != 0 with an infinite / NaN cot keeps the argument, as in Go)
// go: builtinCotSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Real)]
pub fn cot(_ctx: &dyn EvalContext, f: &impl SigView, val: f64) -> Result<Option<f64>, Error> {
    let tan = gomath::tan(val);
    if tan != 0.0 {
        let cot = 1.0 / tan;
        if !cot.is_infinite() && !cot.is_nan() {
            return Ok(Some(cot));
        }
        if f.vector() {
            return Ok(Some(val));
        }
    }
    Err(overflow("DOUBLE", format!("cot({})", fmt_f(val))))
}

// go: degreesFunctionClass.getFunction L1
fn degrees_get_function(
    c: &BaseFunctionClass,
    ctx: &dyn BuildContext,
    args: Vec<ExprRef>,
) -> Result<BuiltinFunc, Error> {
    real_fn(c, ctx, args, 1, &builtinDegreesSig)
}

/// evalReal evals a builtinDegreesSig: `val * 180 / math.Pi`.
// go: builtinDegreesSig.evalReal L1
// go: builtinDegreesSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinDegreesSig.vecEvalReal L2 (the vector driver of this body, M-EVAL)
// go: builtinDegreesSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Real)]
pub fn degrees(_ctx: &dyn EvalContext, _f: &impl SigView, val: f64) -> Result<Option<f64>, Error> {
    Ok(Some(val * 180.0 / std::f64::consts::PI))
}

// go: expFunctionClass.getFunction L1
fn exp_get_function(
    c: &BaseFunctionClass,
    ctx: &dyn BuildContext,
    args: Vec<ExprRef>,
) -> Result<BuiltinFunc, Error> {
    real_fn(c, ctx, args, 1, &builtinExpSig)
}

/// evalReal evals a builtinExpSig.
// go: builtinExpSig.evalReal L1
// go: builtinExpSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinExpSig.vecEvalReal L2 (the vector driver of this body, M-EVAL)
// go: builtinExpSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Real)]
pub fn exp(ctx: &dyn EvalContext, f: &impl SigView, val: f64) -> Result<Option<f64>, Error> {
    let exp = gomath::exp(val);
    if exp.is_infinite() || exp.is_nan() {
        let s = format!(
            "exp({})",
            String::from_utf8_lossy(&f.args[0].string_with_ctx(ctx, RedactLogDisable))
        );
        return Err(overflow("DOUBLE", s));
    }
    Ok(Some(exp))
}

// go: piFunctionClass.getFunction L1
fn pi_get_function(
    c: &BaseFunctionClass,
    ctx: &dyn BuildContext,
    args: Vec<ExprRef>,
) -> Result<BuiltinFunc, Error> {
    c.verify_args(&args)?;
    let mut bf = new_base_builtin_func_with_tp(ctx, c.func_name, args, ETReal, &[])?;
    bf.tp.set_decimal(6);
    bf.tp.set_flen(8);
    Ok(bf.with_sig(&builtinPISig))
}

/// evalReal evals a builtinPISig.
// go: builtinPISig.evalReal L1
// go: builtinPISig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinPISig.vecEvalReal L2 (the vector driver of this body, M-EVAL)
// go: builtinPISig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Real, lazy)]
pub fn pi(
    _ctx: &dyn EvalContext,
    _f: &impl SigView,
    _args: &mut impl Args,
) -> Result<Option<f64>, Error> {
    Ok(Some(std::f64::consts::PI))
}

// go: radiansFunctionClass.getFunction L1
fn radians_get_function(
    c: &BaseFunctionClass,
    ctx: &dyn BuildContext,
    args: Vec<ExprRef>,
) -> Result<BuiltinFunc, Error> {
    real_fn(c, ctx, args, 1, &builtinRadiansSig)
}

/// evalReal evals RADIANS(X): `x * (math.Pi / 180)`.
// go: builtinRadiansSig.evalReal L1
// go: builtinRadiansSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinRadiansSig.vecEvalReal L2 (the vector driver of this body, M-EVAL)
// go: builtinRadiansSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Real)]
pub fn radians(_ctx: &dyn EvalContext, _f: &impl SigView, x: f64) -> Result<Option<f64>, Error> {
    const PI_OVER_180: f64 = f64::from_bits(0x3f91df46a2529d39); // math.Pi / 180
    Ok(Some(x * PI_OVER_180))
}

// go: sinFunctionClass.getFunction L1
fn sin_get_function(
    c: &BaseFunctionClass,
    ctx: &dyn BuildContext,
    args: Vec<ExprRef>,
) -> Result<BuiltinFunc, Error> {
    real_fn(c, ctx, args, 1, &builtinSinSig)
}

/// evalReal evals a builtinSinSig.
// go: builtinSinSig.evalReal L1
// go: builtinSinSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinSinSig.vecEvalReal L2 (the vector driver of this body, M-EVAL)
// go: builtinSinSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Real)]
pub fn sin(_ctx: &dyn EvalContext, _f: &impl SigView, val: f64) -> Result<Option<f64>, Error> {
    Ok(Some(gomath::sin(val)))
}

// go: tanFunctionClass.getFunction L1
fn tan_get_function(
    c: &BaseFunctionClass,
    ctx: &dyn BuildContext,
    args: Vec<ExprRef>,
) -> Result<BuiltinFunc, Error> {
    real_fn(c, ctx, args, 1, &builtinTanSig)
}

/// evalReal evals a builtinTanSig.
// go: builtinTanSig.evalReal L1
// go: builtinTanSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinTanSig.vecEvalReal L2 (the vector driver of this body, M-EVAL)
// go: builtinTanSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Real)]
pub fn tan(_ctx: &dyn EvalContext, _f: &impl SigView, val: f64) -> Result<Option<f64>, Error> {
    Ok(Some(gomath::tan(val)))
}

// ---------------------------------------------------------------- CONV

// go: convFunctionClass.getFunction L1
fn conv_get_function(
    c: &BaseFunctionClass,
    ctx: &dyn BuildContext,
    args: Vec<ExprRef>,
) -> Result<BuiltinFunc, Error> {
    c.verify_args(&args)?;
    let mut bf =
        new_base_builtin_func_with_tp(ctx, c.func_name, args, ETString, &[ETString, ETInt, ETInt])?;
    let (charset, collate) = ctx.get_charset_info();
    bf.tp.set_charset(&charset);
    bf.tp.set_collate(&collate);
    bf.tp.set_flen(64);
    Ok(bf.with_sig(&builtinConvSig))
}

/// The bit string `0b…` of a binary literal, `GetBinaryLiteral().ToBitLiteralString(true)`.
// glue: shared by the two hybrid branches of builtinConvSig.evalString
fn bit_literal_string(d: &tidb_types::datum::Datum) -> Vec<u8> {
    d.get_binary_literal()
        .to_bit_literal_string(true)
        .into_bytes()
}

/// evalString evals CONV(N,from_base,to_base). A binary literal constant, or a CAST of a
/// hybrid / binary literal argument, is first converted from its bit string (base 2); this
/// looks at the argument expressions, so it is a hand-written row driver (the vector
/// version, [`conv_vec`], does not do it, as in Go).
// go: builtinConvSig.evalString L1
// go: builtinConvSig.Clone L2 (BuiltinFunc::clone_from_base)
fn conv_row(
    f: &BuiltinFunc,
    ctx: &dyn EvalContext,
    st: &EvalState,
    row: Row<'_>,
) -> Result<Option<Vec<u8>>, Error> {
    let mut s: Vec<u8> = Vec::new();
    match &*f.args[0] {
        Expr::Constant(x) if x.value.kind() == KindBinaryLiteral => {
            let datum = x.eval(ctx, st, row)?;
            s = bit_literal_string(&datum);
        }
        Expr::ScalarFunction(x) if x.func_name.l == ast::Cast => {
            let arg0 = &x.get_args()[0];
            if arg0.get_type(ctx).hybrid() || is_binary_literal(arg0) {
                let Some(v) = arg0.eval_string(ctx, st, row)? else {
                    return Ok(None);
                };
                s = bit_literal_string(&new_string_datum(v));
            }
        }
        _ => {}
    }
    let Some(from_base) = f.args[1].eval_int(ctx, st, row)? else {
        return Ok(None);
    };
    let Some(to_base) = f.args[2].eval_int(ctx, st, row)? else {
        return Ok(None);
    };
    if s.is_empty() {
        let Some(v) = f.args[0].eval_string(ctx, st, row)? else {
            return Ok(None);
        };
        s = v;
    } else {
        // Go: an invalid from_base gives isNull without an error and goes on with "".
        s = conv(&s[2..], 2, from_base)?.unwrap_or_default();
    }
    conv(&s, from_base, to_base)
}

/// The CONV body without the hybrid / binary literal handling of the row version, kept as
/// the generated drivers' body; only its row driver is replaced (by [`conv_row`]).
/// `novec`: Go's `vecEvalString` is unreachable in v8.5.8 — `vectorized()` is false, so
/// `ScalarFunction.Vectorized()` never calls `isChildrenVectorized`, the buffer allocator
/// stays nil and a direct `VecEvalString` nil-dereferences (D08.5 spec §3.2). The vector
/// driver therefore returns the `baseBuiltinFunc.vecEvalString` error, so Rust has no
/// vector path that no Go record checks.
// go: builtinConvSig.vecEvalString L2 (unreachable in v8.5.8: novec, the base error)
// go: builtinConvSig.vectorized L2 (SigDesc.vectorized = false)
#[eval_fn(String, novec)]
pub fn conv_vec(
    _ctx: &dyn EvalContext,
    _f: &impl SigView,
    s: &[u8],
    from_base: i64,
    to_base: i64,
    out: &mut Vec<u8>,
) -> Result<Option<()>, Error> {
    match conv(s, from_base, to_base)? {
        None => Ok(None),
        Some(v) => {
            out.extend_from_slice(&v);
            Ok(Some(()))
        }
    }
}

/// The drivers of `builtinConvSig`: the hand-written row version and the generated `novec`
/// vector driver of [`conv_vec`].
// glue: SigDrivers of builtinConvSig
const CONV: SigDrivers = match CONV_VEC {
    SigDrivers::String { vec, args, .. } => SigDrivers::String {
        row: conv_row,
        vec,
        args,
    },
    _ => panic!("conv_vec is a String body"),
};

/// `strconv.ParseUint(s, base, 64)` on a prefix `getValidPrefix` accepted (digits of the
/// base only, non-empty): only a range error is possible.
// go-std: strconv.ParseUint L2 (validated digits only)
fn parse_uint_base(s: &[u8], base: u64) -> Option<u64> {
    let mut n: u64 = 0;
    for &c in s {
        let d = match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'z' => c - b'a' + 10,
            b'A'..=b'Z' => c - b'A' + 10,
            _ => return None,
        };
        n = n.checked_mul(base)?.checked_add(u64::from(d))?;
    }
    Some(n)
}

/// `strconv.FormatUint(v, base)`: lower-case digits.
// go-std: strconv.FormatUint L1
fn format_uint(mut v: u64, base: u64) -> Vec<u8> {
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if v == 0 {
        return b"0".to_vec();
    }
    let mut buf = Vec::new();
    while v > 0 {
        buf.push(DIGITS[(v % base) as usize]);
        v /= base;
    }
    buf.reverse();
    buf
}

// go: builtinConvSig.conv L1
fn conv(s: &[u8], mut from_base: i64, mut to_base: i64) -> Result<Option<Vec<u8>>, Error> {
    let mut signed = false;
    let mut ignore_sign = false;
    if from_base < 0 {
        from_base = from_base.wrapping_neg();
        signed = true;
    }
    if to_base < 0 {
        to_base = to_base.wrapping_neg();
        ignore_sign = true;
    }
    if !(2..=36).contains(&from_base) || !(2..=36).contains(&to_base) {
        return Ok(None);
    }
    let mut s = crate::util::get_valid_prefix(trim_space(s), from_base);
    if s.is_empty() {
        return Ok(Some(b"0".to_vec()));
    }
    let mut negative = false;
    if s[0] == b'-' {
        negative = true;
        s = &s[1..];
    }
    let Some(mut val) = parse_uint_base(s, from_base as u64) else {
        return Err(overflow(
            "BIGINT UNSIGNED",
            String::from_utf8_lossy(s).into_owned(),
        ));
    };
    if signed {
        const NEG_MIN_INT64: u64 = 1 << 63; // -math.MinInt64
        if negative && val > NEG_MIN_INT64 {
            val = NEG_MIN_INT64;
        }
        if !negative && val > i64::MAX as u64 {
            val = i64::MAX as u64;
        }
    }
    if negative {
        val = val.wrapping_neg();
    }
    negative = (val as i64) < 0;
    if ignore_sign && negative {
        val = 0u64.wrapping_sub(val);
    }
    let mut out = format_uint(val, to_base as u64);
    if negative && ignore_sign {
        out.insert(0, b'-');
    }
    out.make_ascii_uppercase();
    Ok(Some(out))
}

// ---------------------------------------------------------------- CRC32

// go: crc32FunctionClass.getFunction L1
fn crc32_get_function(
    c: &BaseFunctionClass,
    ctx: &dyn BuildContext,
    args: Vec<ExprRef>,
) -> Result<BuiltinFunc, Error> {
    c.verify_args(&args)?;
    let mut bf = new_base_builtin_func_with_tp(ctx, c.func_name, args, ETInt, &[ETString])?;
    bf.tp.set_flen(10);
    bf.tp.add_flag(mysql::UnsignedFlag);
    Ok(bf.with_sig(&builtinCRC32Sig))
}

/// evalInt evals a CRC32(expr).
// go: builtinCRC32Sig.evalInt L1
// go: builtinCRC32Sig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinCRC32Sig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
// go: builtinCRC32Sig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Int)]
pub fn crc32(_ctx: &dyn EvalContext, _f: &impl SigView, x: &[u8]) -> Result<Option<i64>, Error> {
    Ok(Some(i64::from(checksum_ieee(x))))
}

/// `builtinConvSig`: Go's `vectorized()` is false (TODO in Go: the vector version lacks
/// the hybrid handling of the row version).
pub static builtinConvSig: SigDesc = SigDesc {
    name: "builtinConvSig",
    pb_code: 2138,
    vectorized: false,
    drivers: CONV,
    args_fallback: crate::eval::ArgsFallback::None,
};
math_sig!(builtinCRC32Sig, 2139, CRC32);

// tipb.ScalarFuncSig codes (go-tipb expression.pb.go).
math_sig!(builtinAbsIntSig, 2101, ABS_INT);
math_sig!(builtinAbsUIntSig, 2102, ABS_UINT);
math_sig!(builtinAbsRealSig, 2103, ABS_REAL);
math_sig!(builtinAbsDecSig, 2104, ABS_DEC);
math_sig!(builtinRoundRealSig, 2121, ROUND_REAL);
math_sig!(builtinRoundIntSig, 2122, ROUND_INT);
math_sig!(builtinRoundDecSig, 2123, ROUND_DEC);
math_sig!(builtinRoundWithFracRealSig, 2124, ROUND_WITH_FRAC_REAL);
math_sig!(builtinRoundWithFracIntSig, 2125, ROUND_WITH_FRAC_INT);
math_sig!(builtinRoundWithFracDecSig, 2126, ROUND_WITH_FRAC_DEC);
math_sig!(builtinLog1ArgSig, 2131, LOG1_ARG);
math_sig!(builtinLog2ArgsSig, 2132, LOG2_ARGS);
math_sig!(builtinLog2Sig, 2133, LOG2);
math_sig!(builtinLog10Sig, 2134, LOG10);
math_sig!(builtinRandSig, 2135, RAND);
math_sig!(
    builtinRandWithSeedFirstGenSig,
    2136,
    RAND_WITH_SEED_FIRST_GEN
);
math_sig!(builtinPowSig, 2137, POW);
math_sig!(builtinSignSig, 2140, SIGN);
math_sig!(builtinSqrtSig, 2141, SQRT);
math_sig!(builtinAcosSig, 2142, ACOS);
math_sig!(builtinAsinSig, 2143, ASIN);
math_sig!(builtinAtan1ArgSig, 2144, ATAN1_ARG);
math_sig!(builtinAtan2ArgsSig, 2145, ATAN2_ARGS);
math_sig!(builtinCosSig, 2146, COS);
math_sig!(builtinCotSig, 2147, COT);
math_sig!(builtinDegreesSig, 2148, DEGREES);
math_sig!(builtinExpSig, 2149, EXP);
math_sig!(builtinPISig, 2150, PI);
math_sig!(builtinRadiansSig, 2151, RADIANS);
math_sig!(builtinSinSig, 2152, SIN);
math_sig!(builtinTanSig, 2153, TAN);
math_sig!(builtinTruncateIntSig, 2154, TRUNCATE_INT);
math_sig!(builtinTruncateRealSig, 2155, TRUNCATE_REAL);
math_sig!(builtinTruncateDecimalSig, 2156, TRUNCATE_DECIMAL);
math_sig!(builtinTruncateUintSig, 2157, TRUNCATE_UINT);
