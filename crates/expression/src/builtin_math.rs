//! go-file: pkg/expression/builtin_math.go
//! go-file: pkg/expression/builtin_math_vec.go
//!
//! `builtin_math.go` (D08.5), first batch: CEIL / CEILING and FLOOR (the two function
//! classes and their ten signatures). `RefineComparedConstant` (D08.4) builds them through
//! `NewFunctionInternal`; the other math functions are later batches (specs/D08.5-arith).
#![allow(non_upper_case_globals)]

use tidb_base::Error;
use tidb_base::mysql::{self, MaxIntWidth, TypeLong, TypeLonglong, TypeNewDecimal};
use tidb_parser::types::{ETDecimal, ETInt, ETReal, EvalType};
use tidb_types::errors::ErrTruncated;
use tidb_types::etc::FieldType;
use tidb_types::mydecimal::{
    MODE_TRUNCATE, MyDecimal, decimal_add, decimal_sub, new_dec_from_int, new_dec_from_uint,
};

use crate::ast_funcs as ast;
use crate::builtin::{BaseFunctionClass, new_base_builtin_func_with_tp};
use crate::builtin_registry::FunctionClass;
use crate::context::{BuildContext, EvalContext};
use crate::eval::{BuiltinFunc, SigDesc, SigView};
use crate::expression::{Expr, ExprRef};
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
