//! go-file: pkg/expression/builtin_cast.go
//!
//! CAST (block D08.8), first batch: the function classes of CAST to INT, REAL and DECIMAL,
//! their signatures from INT, REAL, DECIMAL and STRING arguments, `BuildCastFunction*` and
//! `WrapWithCastAsInt`/`Real`/`Decimal` (spec D08.8 §1).
//!
//! The signatures from TIME, DURATION, JSON and VECTOR arguments are selected by the function
//! classes (so the planner sees Go's signature, result type and `tipb` code) but have no
//! drivers yet: temporary stubs (constitution art. 4.3), evaluating them returns Go's
//! `baseBuiltinFunc` error. CAST to STRING/TIME/DURATION/JSON/VECTOR returns a "not ported
//! yet" error from `build_cast_function*` (D08.8 part 2).

#![allow(non_upper_case_globals, non_snake_case)]

use std::sync::Arc;

use tidb_base::Error;
use tidb_base::errors::RedactLogDisable;
use tidb_base::gofmt::Arg;
use tidb_base::gostd::bytes::trim_space;
use tidb_base::mysql::{
    self, EnumSetAsIntFlag, MaxDecimalScale, MaxDecimalWidth, MaxIntWidth, MaxRealWidth,
    NotFixedDec, NotNullFlag, TypeDouble, TypeEnum, TypeInt24, TypeLong, TypeLonglong,
    TypeNewDecimal, TypeShort, TypeTiny, TypeYear, UnsignedFlag, has_not_null_flag,
    has_unsigned_flag,
};
use tidb_parser::types::{
    ETDatetime, ETDecimal, ETDuration, ETInt, ETJson, ETReal, ETString, ETTimestamp,
    ETVectorFloat32, EvalType, UnspecifiedLength,
};
use tidb_types::convert::{
    convert_float_to_int, convert_float_to_uint, integer_signed_lower_bound,
    integer_signed_upper_bound, integer_unsigned_upper_bound, str_to_float, str_to_int,
    str_to_uint,
};
use tidb_types::datum::{produce_dec_with_specified_tp, produce_float_with_specified_tp};
use tidb_types::errors::{
    ErrCastAsSignedOverflow, ErrCastNegIntAsUnsigned, ErrOverflow, ErrTruncated,
    ErrTruncatedWrongVal,
};
use tidb_types::etc::FieldType;
use tidb_types::field_type::{new_field_type, set_bin_chs_cln_flag};
use tidb_types::mydecimal::{MODE_HALF_UP, MyDecimal, new_dec_from_int, new_dec_from_uint};

use crate::ast_funcs as ast;
use crate::builtin::{BaseFunctionClass, new_base_builtin_cast_func, new_base_builtin_func};
use crate::constant_fold::fold_constant;
use crate::context::{BuildContext, EvalContext, err_ctx, type_ctx};
use crate::eval::{BuiltinFunc, SigDesc, SigDrivers, SigView};
use crate::expression::{ConstStrict, EvalState, Expr, ExprRef, is_binary_literal};
use crate::scalar_function::ScalarFunction;
use tidb_expression_macros::eval_fn;

// ---------------------------------------------------------------- tipb codes

// tipb.ScalarFuncSig of the CAST signatures (go-tipb expression.pb.go).
const CastIntAsInt: i32 = 1;
const CastIntAsReal: i32 = 2;
const CastIntAsDecimal: i32 = 4;
const CastRealAsInt: i32 = 10;
const CastRealAsReal: i32 = 11;
const CastRealAsDecimal: i32 = 13;
const CastDecimalAsInt: i32 = 20;
const CastDecimalAsReal: i32 = 21;
const CastDecimalAsDecimal: i32 = 23;
const CastStringAsInt: i32 = 30;
const CastStringAsReal: i32 = 31;
const CastStringAsDecimal: i32 = 33;
const CastTimeAsInt: i32 = 40;
const CastTimeAsReal: i32 = 41;
const CastTimeAsDecimal: i32 = 43;
const CastDurationAsInt: i32 = 50;
const CastDurationAsReal: i32 = 51;
const CastDurationAsDecimal: i32 = 53;
const CastJsonAsInt: i32 = 60;
const CastJsonAsReal: i32 = 61;
const CastJsonAsDecimal: i32 = 63;

// ---------------------------------------------------------------- signatures

macro_rules! sig {
    ($name:ident, $pb:expr, $vec:expr, $drivers:expr) => {
        #[doc = concat!("`", stringify!($name), "`.")]
        pub static $name: SigDesc = SigDesc {
            name: stringify!($name),
            pb_code: $pb,
            vectorized: $vec,
            drivers: $drivers,
        };
    };
}

sig!(builtinCastIntAsIntSig, CastIntAsInt, true, CAST_INT_AS_INT);
sig!(builtinCastIntAsRealSig, CastIntAsReal, true, CAST_INT_AS_REAL);
sig!(builtinCastIntAsDecimalSig, CastIntAsDecimal, true, CAST_INT_AS_DECIMAL);
sig!(builtinCastRealAsIntSig, CastRealAsInt, true, CAST_REAL_AS_INT);
sig!(builtinCastRealAsRealSig, CastRealAsReal, true, CAST_REAL_AS_REAL);
sig!(builtinCastRealAsDecimalSig, CastRealAsDecimal, true, CAST_REAL_AS_DECIMAL);
sig!(builtinCastDecimalAsIntSig, CastDecimalAsInt, true, CAST_DECIMAL_AS_INT);
sig!(builtinCastDecimalAsRealSig, CastDecimalAsReal, true, CAST_DECIMAL_AS_REAL);
sig!(builtinCastDecimalAsDecimalSig, CastDecimalAsDecimal, true, CAST_DECIMAL_AS_DECIMAL);
sig!(builtinCastStringAsIntSig, CastStringAsInt, true, CAST_STRING_AS_INT);
sig!(builtinCastStringAsRealSig, CastStringAsReal, true, CAST_STRING_AS_REAL);
sig!(builtinCastStringAsDecimalSig, CastStringAsDecimal, true, CAST_STRING_AS_DECIMAL);

// Temporary stubs (D08.8 part 2): selected by the function classes, not evaluated yet.
sig!(builtinCastTimeAsIntSig, CastTimeAsInt, true, SigDrivers::None);
sig!(builtinCastTimeAsRealSig, CastTimeAsReal, true, SigDrivers::None);
sig!(builtinCastTimeAsDecimalSig, CastTimeAsDecimal, true, SigDrivers::None);
sig!(builtinCastDurationAsIntSig, CastDurationAsInt, true, SigDrivers::None);
sig!(builtinCastDurationAsRealSig, CastDurationAsReal, true, SigDrivers::None);
sig!(builtinCastDurationAsDecimalSig, CastDurationAsDecimal, true, SigDrivers::None);
sig!(builtinCastJSONAsIntSig, CastJsonAsInt, true, SigDrivers::None);
sig!(builtinCastJSONAsRealSig, CastJsonAsReal, true, SigDrivers::None);
sig!(builtinCastJSONAsDecimalSig, CastJsonAsDecimal, true, SigDrivers::None);
// Go does not call setPbCode for it (the code is commented out).
sig!(builtinCastVectorFloat32AsUnsupportedSig, 0, false, SigDrivers::None);

/// `b.inUnion` of a `baseBuiltinCastFunc`.
#[inline(always)]
// glue: read of the embedded baseBuiltinCastFunc.inUnion
fn in_union(f: &BuiltinFunc) -> bool {
    f.in_union == Some(true)
}

/// The decimal result of a signature after `ProduceDecWithSpecifiedTp` and
/// `ec.HandleError(err)`. Go returns a nil `*MyDecimal` with `ErrMBiggerThanD`; a caller
/// that went on after the error was turned into a warning would dereference it.
// glue: the shared tail `res, err = ProduceDecWithSpecifiedTp(..); err = ec.HandleError(err)` of the decimal signatures
fn produce_dec(
    ctx: &dyn EvalContext,
    f: &BuiltinFunc,
    res: MyDecimal,
) -> Result<Option<MyDecimal>, Error> {
    let (res, err) = produce_dec_with_specified_tp(&type_ctx(ctx), res, &f.tp);
    if let Err(e) = err {
        err_ctx(ctx).handle_error(e)?;
    }
    Ok(Some(res.expect("nil *MyDecimal from ProduceDecWithSpecifiedTp")))
}

/// `builtinCastIntAsIntSig.evalInt`.
// go: builtinCastIntAsIntSig.evalInt L1
// go: builtinCastIntAsIntSig.Clone L2 (BuiltinFunc::clone_from_base)
#[eval_fn(Int)]
pub fn cast_int_as_int(
    _ctx: &dyn EvalContext,
    f: &impl SigView,
    res: i64,
) -> Result<Option<i64>, Error> {
    let mut res = res;
    if in_union(f) && has_unsigned_flag(f.tp.get_flag()) && res < 0 {
        res = 0;
    }
    Ok(Some(res))
}

// go: builtinCastIntAsRealSig.evalReal L1
// go: builtinCastIntAsRealSig.Clone L2 (BuiltinFunc::clone_from_base)
#[eval_fn(Real)]
pub fn cast_int_as_real(
    _ctx: &dyn EvalContext,
    f: &impl SigView,
    val: i64,
) -> Result<Option<f64>, Error> {
    let unsigned_args0 = has_unsigned_flag(f.arg_type(0).get_flag());
    let res = if !has_unsigned_flag(f.tp.get_flag()) && !unsigned_args0 {
        val as f64
    } else if in_union(f) && !unsigned_args0 && val < 0 {
        // Round up to 0 if the value is negative but the expression eval type is unsigned
        // in `UNION` statement.
        0.0
    } else {
        // recall that, int to float is different from uint to float
        val as u64 as f64
    };
    Ok(Some(res))
}

// go: builtinCastIntAsDecimalSig.evalDecimal L1
// go: builtinCastIntAsDecimalSig.Clone L2 (BuiltinFunc::clone_from_base)
#[eval_fn(Decimal)]
pub fn cast_int_as_decimal(
    ctx: &dyn EvalContext,
    f: &impl SigView,
    val: i64,
) -> Result<Option<MyDecimal>, Error> {
    let unsigned_args0 = has_unsigned_flag(f.arg_type(0).get_flag());
    let res = if !has_unsigned_flag(f.tp.get_flag()) && !unsigned_args0 {
        new_dec_from_int(val)
    } else if in_union(f) && !unsigned_args0 && val < 0 {
        MyDecimal::default()
    } else {
        new_dec_from_uint(val as u64)
    };
    produce_dec(ctx, f, res)
}

// go: builtinCastRealAsRealSig.evalReal L1
// go: builtinCastRealAsRealSig.Clone L2 (BuiltinFunc::clone_from_base)
#[eval_fn(Real)]
pub fn cast_real_as_real(
    _ctx: &dyn EvalContext,
    f: &impl SigView,
    res: f64,
) -> Result<Option<f64>, Error> {
    let mut res = res;
    if in_union(f) && has_unsigned_flag(f.tp.get_flag()) && res < 0.0 {
        res = 0.0;
    }
    Ok(Some(res))
}

// go: builtinCastRealAsIntSig.evalInt L1
// go: builtinCastRealAsIntSig.Clone L2 (BuiltinFunc::clone_from_base)
#[eval_fn(Int)]
pub fn cast_real_as_int(
    ctx: &dyn EvalContext,
    f: &impl SigView,
    val: f64,
) -> Result<Option<i64>, Error> {
    let (res, err) = if !has_unsigned_flag(f.tp.get_flag()) {
        convert_float_to_int(
            val,
            integer_signed_lower_bound(TypeLonglong),
            integer_signed_upper_bound(TypeLonglong),
            TypeLonglong,
        )
    } else if in_union(f) && val < 0.0 {
        (0, Ok(()))
    } else {
        let tc = type_ctx(ctx);
        let (uint_val, err) = convert_float_to_uint(
            tc.flags(),
            val,
            integer_unsigned_upper_bound(TypeLonglong),
            TypeLonglong,
        );
        (uint_val as i64, err)
    };
    match err {
        Err(e) if ErrOverflow.equal(&e) => err_ctx(ctx).handle_error(e)?,
        other => other?,
    }
    Ok(Some(res))
}

// go: builtinCastRealAsDecimalSig.evalDecimal L1
// go: builtinCastRealAsDecimalSig.Clone L2 (BuiltinFunc::clone_from_base)
#[eval_fn(Decimal)]
pub fn cast_real_as_decimal(
    ctx: &dyn EvalContext,
    f: &impl SigView,
    val: f64,
) -> Result<Option<MyDecimal>, Error> {
    let mut res = MyDecimal::default();
    let ec = err_ctx(ctx);
    if !in_union(f) || val >= 0.0 {
        match res.from_float64(val) {
            Ok(()) => {}
            Err(e) if ErrOverflow.equal(&e) => {
                let warn_err = ErrTruncatedWrongVal.gen_with_stack_by_args(vec![
                    Arg::from("DECIMAL"),
                    Arg::Str(f.args[0].string_with_ctx(ctx, RedactLogDisable).into()),
                ]);
                ec.handle_error_with_alias(e.clone(), e, warn_err)?;
            }
            // This behavior is consistent with MySQL.
            Err(e) if ErrTruncated.equal(&e) => {}
            Err(e) => return Err(e),
        }
    }
    produce_dec(ctx, f, res)
}

// go: builtinCastDecimalAsDecimalSig.evalDecimal L1
// go: builtinCastDecimalAsDecimalSig.Clone L2 (BuiltinFunc::clone_from_base)
#[eval_fn(Decimal)]
pub fn cast_decimal_as_decimal(
    ctx: &dyn EvalContext,
    f: &impl SigView,
    eval_decimal: &MyDecimal,
) -> Result<Option<MyDecimal>, Error> {
    let mut res = MyDecimal::default();
    if !(in_union(f) && has_unsigned_flag(f.tp.get_flag()) && eval_decimal.is_negative()) {
        res = *eval_decimal;
    }
    produce_dec(ctx, f, res)
}

// go: builtinCastDecimalAsIntSig.evalInt L1
// go: builtinCastDecimalAsIntSig.Clone L2 (BuiltinFunc::clone_from_base)
#[eval_fn(Int)]
pub fn cast_decimal_as_int(
    ctx: &dyn EvalContext,
    f: &impl SigView,
    val: &MyDecimal,
) -> Result<Option<i64>, Error> {
    // Round is needed for both unsigned and signed.
    let mut to = MyDecimal::default();
    val.round(&mut to, 0, MODE_HALF_UP)?;

    let (res, err) = if !has_unsigned_flag(f.tp.get_flag()) {
        to.to_int()
    } else if in_union(f) && to.is_negative() {
        (0, Ok(()))
    } else {
        let (uint_res, err) = to.to_uint();
        (uint_res as i64, err)
    };
    match err {
        Err(e) if ErrOverflow.equal(&e) => {
            let warn_err = ErrTruncatedWrongVal.gen_with_stack_by_args(vec![
                Arg::from("DECIMAL"),
                Arg::Stringer(val.to_string(), "*types.MyDecimal"),
            ]);
            err_ctx(ctx).handle_error_with_alias(e.clone(), e, warn_err)?;
        }
        other => other?,
    }
    Ok(Some(res))
}

// go: builtinCastDecimalAsRealSig.evalReal L1
// go: builtinCastDecimalAsRealSig.Clone L2 (BuiltinFunc::clone_from_base)
#[eval_fn(Real)]
pub fn cast_decimal_as_real(
    _ctx: &dyn EvalContext,
    f: &impl SigView,
    val: &MyDecimal,
) -> Result<Option<f64>, Error> {
    if in_union(f) && has_unsigned_flag(f.tp.get_flag()) && val.is_negative() {
        return Ok(Some(0.0));
    }
    let (res, err) = val.to_float64();
    err?;
    Ok(Some(res))
}

/// handleOverflow handles the overflow caused by cast string as int: the endpoint of the
/// range with a warning (MySQL's behavior for out-of-range values in SELECT).
// go: builtinCastStringAsIntSig.handleOverflow L1
fn handle_overflow(
    ctx: &dyn EvalContext,
    orig_res: i64,
    orig_str: &[u8],
    orig_err: Result<(), Error>,
    is_negative: bool,
) -> (i64, Result<(), Error>) {
    let Err(e) = orig_err else {
        return (orig_res, Ok(()));
    };
    if ErrOverflow.equal(&e) {
        let res = if is_negative { i64::MIN } else { u64::MAX as i64 };
        let warn_err = ErrTruncatedWrongVal.gen_with_stack_by_args(vec![
            Arg::from("INTEGER"),
            Arg::Str(orig_str.to_vec().into()),
        ]);
        let err = err_ctx(ctx).handle_error_with_alias(e.clone(), e, warn_err);
        return (res, err);
    }
    (orig_res, Err(e))
}

/// Go first sends hybrid (ENUM/SET/BIT) arguments, binary literals and functions with an
/// implicit int path (`CanImplicitEvalInt`) to `args[0].EvalInt`; the function class never
/// builds this signature over such an argument (it picks `builtinCastIntAsIntSig`), so the
/// body starts at the string path (spec §4).
// go: builtinCastStringAsIntSig.evalInt L2 (the hybrid/binary-literal/implicit-int prologue is unreachable from castAsIntFunctionClass)
// go: builtinCastStringAsIntSig.Clone L2 (BuiltinFunc::clone_from_base)
#[eval_fn(Int)]
pub fn cast_string_as_int(
    ctx: &dyn EvalContext,
    f: &impl SigView,
    val: &[u8],
) -> Result<Option<i64>, Error> {
    let val = trim_space(val);
    let is_negative = val.len() > 1 && val[0] == b'-'; // negative number

    let tc = type_ctx(ctx);
    let unsigned = has_unsigned_flag(f.tp.get_flag());
    let (res, err) = if !is_negative {
        let (ures, err) = str_to_uint(&tc, val, true);
        if err.is_ok() && !unsigned && ures > i64::MAX as u64 {
            tc.append_warning(ErrCastAsSignedOverflow.clone());
        }
        (ures as i64, err)
    } else if in_union(f) && unsigned {
        (0, Ok(()))
    } else {
        let (res, err) = str_to_int(&tc, val, true);
        if err.is_ok() && unsigned {
            // If overflow, don't append this warnings
            tc.append_warning(ErrCastNegIntAsUnsigned.clone());
        }
        (res, err)
    };

    let (res, err) = handle_overflow(ctx, res, val, err, is_negative);
    err?;
    Ok(Some(res))
}

// go: builtinCastStringAsRealSig.evalReal L2 (the binary-literal/implicit-real prologue is unreachable from castAsRealFunctionClass)
// go: builtinCastStringAsRealSig.Clone L2 (BuiltinFunc::clone_from_base)
#[eval_fn(Real)]
pub fn cast_string_as_real(
    ctx: &dyn EvalContext,
    f: &impl SigView,
    val: &[u8],
) -> Result<Option<f64>, Error> {
    let (mut res, err) = str_to_float(&type_ctx(ctx), val, true);
    err?;
    if in_union(f) && has_unsigned_flag(f.tp.get_flag()) && res < 0.0 {
        res = 0.0;
    }
    let (res, err) = produce_float_with_specified_tp(res, &f.tp);
    err?;
    Ok(Some(res))
}

// go: builtinCastStringAsDecimalSig.evalDecimal L2 (the binary-literal prologue is unreachable from castAsDecimalFunctionClass)
// go: builtinCastStringAsDecimalSig.Clone L2 (BuiltinFunc::clone_from_base)
#[eval_fn(Decimal)]
pub fn cast_string_as_decimal(
    ctx: &dyn EvalContext,
    f: &impl SigView,
    val: &[u8],
) -> Result<Option<MyDecimal>, Error> {
    let val = trim_space(val);
    let is_negative = val.len() > 1 && val[0] == b'-';
    let mut res = MyDecimal::default();
    let ec = err_ctx(ctx);
    if !(in_union(f) && has_unsigned_flag(f.tp.get_flag()) && is_negative) {
        let err = match res.from_string(val) {
            Err(e) if ErrTruncated.equal(&e) => Err(ErrTruncatedWrongVal
                .gen_with_stack_by_args(vec![Arg::from("DECIMAL"), Arg::Bytes(val.to_vec())])),
            other => other,
        };
        if let Err(e) = err {
            ec.handle_error(e)?;
        }
    }
    produce_dec(ctx, f, res)
}

// ---------------------------------------------------------------- function classes

/// `castAsIntFunctionClass`.
#[derive(Clone, Debug)]
pub struct CastAsIntFunctionClass {
    /// The embedded `baseFunctionClass`.
    pub base: BaseFunctionClass,
    /// `tp`.
    pub tp: FieldType,
    /// `inUnion`.
    pub in_union: bool,
}

/// `castAsRealFunctionClass`.
#[derive(Clone, Debug)]
pub struct CastAsRealFunctionClass {
    /// The embedded `baseFunctionClass`.
    pub base: BaseFunctionClass,
    /// `tp`.
    pub tp: FieldType,
    /// `inUnion`.
    pub in_union: bool,
}

/// `castAsDecimalFunctionClass`.
#[derive(Clone, Debug)]
pub struct CastAsDecimalFunctionClass {
    /// The embedded `baseFunctionClass`.
    pub base: BaseFunctionClass,
    /// `tp`.
    pub tp: FieldType,
    /// `inUnion`.
    pub in_union: bool,
}

/// `bf.baseBuiltinFunc`: the base without the cast part.
// glue: field access bf.baseBuiltinFunc of a baseBuiltinCastFunc
fn base_of(mut bf: BuiltinFunc) -> BuiltinFunc {
    bf.in_union = None;
    bf
}

/// `errors.Errorf("cannot cast from %s to %s", argTp, to)`.
// glue: the default branch of the castAsXxxFunctionClass switches
fn cannot_cast(arg_tp: EvalType, to: &str) -> Error {
    Error::plain(format!("cannot cast from {} to {}", arg_tp.to_string(), to))
}

impl CastAsIntFunctionClass {
    // go: castAsIntFunctionClass.getFunction L1
    pub fn get_function(
        &self,
        ctx: &dyn BuildContext,
        args: Vec<ExprRef>,
    ) -> Result<BuiltinFunc, Error> {
        self.base.verify_args(&args)?;
        let b = new_base_builtin_func(ctx, self.base.func_name, args, self.tp.clone())?;
        let bf = new_base_builtin_cast_func(b, self.in_union);
        let ectx = ctx.get_eval_ctx();
        let arg0_tp = bf.args[0].get_type(&*ectx).into_owned();
        if arg0_tp.hybrid() || is_binary_literal(&bf.args[0]) {
            return Ok(bf.with_sig(&builtinCastIntAsIntSig));
        }
        let arg_tp = arg0_tp.eval_type();
        Ok(match arg_tp {
            ETInt => bf.with_sig(&builtinCastIntAsIntSig),
            ETReal => bf.with_sig(&builtinCastRealAsIntSig),
            ETDecimal => bf.with_sig(&builtinCastDecimalAsIntSig),
            ETDatetime | ETTimestamp => bf.with_sig(&builtinCastTimeAsIntSig),
            ETDuration => bf.with_sig(&builtinCastDurationAsIntSig),
            ETJson => bf.with_sig(&builtinCastJSONAsIntSig),
            ETString => bf.with_sig(&builtinCastStringAsIntSig),
            ETVectorFloat32 => {
                // &builtinCastVectorFloat32AsUnsupportedSig{bf.baseBuiltinFunc}
                base_of(bf)
                    .with_sig(&builtinCastVectorFloat32AsUnsupportedSig)
            }
            _ => return Err(cannot_cast(arg_tp, "Int")),
        })
    }
}

impl CastAsRealFunctionClass {
    // go: castAsRealFunctionClass.getFunction L2 (PropagateType replaces a shared argument node instead of changing it in place, spec §4)
    pub fn get_function(
        &self,
        ctx: &dyn BuildContext,
        args: Vec<ExprRef>,
    ) -> Result<BuiltinFunc, Error> {
        self.base.verify_args(&args)?;
        let b = new_base_builtin_func(ctx, self.base.func_name, args, self.tp.clone())?;
        let mut bf = new_base_builtin_cast_func(b, self.in_union);
        if is_binary_literal(&bf.args[0]) {
            return Ok(bf.with_sig(&builtinCastRealAsRealSig));
        }
        let ectx = ctx.get_eval_ctx();
        let arg0_tp = bf.args[0].get_type(&*ectx).into_owned();
        let arg_tp = if arg0_tp.hybrid() {
            ETInt
        } else {
            arg0_tp.eval_type()
        };
        Ok(match arg_tp {
            ETInt => bf.with_sig(&builtinCastIntAsRealSig),
            ETReal => bf.with_sig(&builtinCastRealAsRealSig),
            ETDecimal => {
                propagate_type(&*ectx, ETReal, &mut bf.args);
                bf.with_sig(&builtinCastDecimalAsRealSig)
            }
            ETDatetime | ETTimestamp => bf.with_sig(&builtinCastTimeAsRealSig),
            ETDuration => bf.with_sig(&builtinCastDurationAsRealSig),
            ETJson => bf.with_sig(&builtinCastJSONAsRealSig),
            ETString => bf.with_sig(&builtinCastStringAsRealSig),
            ETVectorFloat32 => base_of(bf)
                .with_sig(&builtinCastVectorFloat32AsUnsupportedSig),
            _ => return Err(cannot_cast(arg_tp, "Real")),
        })
    }
}

impl CastAsDecimalFunctionClass {
    // go: castAsDecimalFunctionClass.getFunction L1
    pub fn get_function(
        &self,
        ctx: &dyn BuildContext,
        args: Vec<ExprRef>,
    ) -> Result<BuiltinFunc, Error> {
        self.base.verify_args(&args)?;
        let b = new_base_builtin_func(ctx, self.base.func_name, args, self.tp.clone())?;
        let bf = new_base_builtin_cast_func(b, self.in_union);
        if is_binary_literal(&bf.args[0]) {
            return Ok(bf.with_sig(&builtinCastDecimalAsDecimalSig));
        }
        let ectx = ctx.get_eval_ctx();
        let arg0_tp = bf.args[0].get_type(&*ectx).into_owned();
        let arg_tp = if arg0_tp.hybrid() {
            ETInt
        } else {
            arg0_tp.eval_type()
        };
        Ok(match arg_tp {
            ETInt => bf.with_sig(&builtinCastIntAsDecimalSig),
            ETReal => bf.with_sig(&builtinCastRealAsDecimalSig),
            ETDecimal => bf.with_sig(&builtinCastDecimalAsDecimalSig),
            ETDatetime | ETTimestamp => bf.with_sig(&builtinCastTimeAsDecimalSig),
            ETDuration => bf.with_sig(&builtinCastDurationAsDecimalSig),
            ETJson => bf.with_sig(&builtinCastJSONAsDecimalSig),
            ETString => bf.with_sig(&builtinCastStringAsDecimalSig),
            ETVectorFloat32 => base_of(bf)
                .with_sig(&builtinCastVectorFloat32AsUnsupportedSig),
            _ => return Err(cannot_cast(arg_tp, "Decimal")),
        })
    }
}

// ---------------------------------------------------------------- type helpers

/// setDataTypeDouble: the flen and decimal of a DOUBLE converted from a value with
/// `src_decimal` fractional digits.
// go: setDataTypeDouble L1
pub(crate) fn set_data_type_double(src_decimal: isize) -> (isize, isize) {
    let decimal = NotFixedDec;
    let flen = float_length(src_decimal, decimal);
    (flen, decimal)
}

// go: floatLength L1
pub(crate) fn float_length(src_decimal: isize, decimal_par: isize) -> isize {
    const dblDIG: isize = 15;
    if src_decimal != NotFixedDec {
        return dblDIG + 2 + decimal_par;
    }
    dblDIG + 8
}

/// Changes the result type of `e` (Go's `e.GetType(ctx).SetXxx(..)` on the shared node). A
/// node shared with other holders is copied first (they keep the old type; Go changes the
/// one node everybody holds, spec §4). A scalar function's `RetType` and its signature's `tp`
/// are one `*FieldType` in Go, so both change. A constant over a parameter marker has a type
/// computed on each `GetType`, so Go's change is lost: nothing changes here either.
// glue: in-place change of a node's *FieldType (Arc nodes are immutable, M-EXPR)
pub(crate) fn with_type_mut(e: &mut ExprRef, f: impl FnOnce(&mut FieldType)) {
    match Arc::make_mut(e) {
        Expr::Column(c) => f(&mut c.ret_type),
        Expr::CorrelatedColumn(c) => f(&mut c.column.ret_type),
        Expr::Constant(c) => {
            if c.param_marker.is_none() {
                f(&mut c.ret_type);
            }
        }
        Expr::ScalarFunction(sf) => {
            f(&mut sf.ret_type);
            sf.function.tp = sf.ret_type.clone();
            sf.hashcode = Default::default();
            sf.canonicalhashcode = Default::default();
        }
    }
}

/// PropagateType change the type of the argument according to the evaluation type of the
/// function (only ETReal changes a DECIMAL argument's flen and decimal).
// go: PropagateType L2 (a shared argument node is copied before its type changes, spec §4)
pub fn propagate_type(ctx: &dyn EvalContext, eval_type: EvalType, args: &mut [ExprRef]) {
    if eval_type != ETReal {
        return;
    }
    let tp = args[0].get_type(ctx).into_owned();
    let (old_flen, old_decimal) = (tp.get_flen(), tp.get_decimal());
    let (mut new_flen, mut new_decimal) = set_data_type_double(tp.get_decimal());
    // For float(M,D), double(M,D) or decimal(M,D), M must be >= D.
    if new_flen < new_decimal {
        new_flen = old_flen - old_decimal + new_decimal;
    }
    if old_flen != new_flen || old_decimal != new_decimal {
        // Go clones a Column / CorrelatedColumn and its RetType here; with_type_mut copies a
        // shared node.
        if tp.get_type() == TypeNewDecimal {
            if new_decimal > MaxDecimalScale {
                new_decimal = MaxDecimalScale;
            }
            if old_flen - old_decimal > new_flen - new_decimal {
                // the input data should never be overflow under the new type
                if new_decimal > old_decimal {
                    // if the target decimal part is larger than the original decimal part, we
                    // try to extend the decimal part as much as possible while keeping the
                    // integer part big enough to hold the original data.
                    let inc_decimal = (new_decimal - old_decimal).min(MaxDecimalWidth - old_flen);
                    new_flen = old_flen + inc_decimal;
                    new_decimal = old_decimal + inc_decimal;
                } else {
                    (new_flen, new_decimal) = (old_flen, old_decimal);
                }
            }
        }
        with_type_mut(&mut args[0], |t| {
            t.set_flen_under_limit(new_flen);
            t.set_decimal_under_limit(new_decimal);
        });
    }
}

// ---------------------------------------------------------------- building CAST

/// CanImplicitEvalInt represents the builtin functions that have an implicit path to
/// evaluate as integer, regardless of the type that type inference decides it to be
/// (MySQL's `dayname()`).
// go: CanImplicitEvalInt L1
pub fn can_implicit_eval_int(expr: &Expr) -> bool {
    match expr {
        Expr::ScalarFunction(f) => f.func_name.l == ast::DayName,
        _ => false,
    }
}

/// CanImplicitEvalReal: as [`can_implicit_eval_int`], for real.
// go: CanImplicitEvalReal L1
pub fn can_implicit_eval_real(expr: &Expr) -> bool {
    match expr {
        Expr::ScalarFunction(f) => f.func_name.l == ast::DayName,
        _ => false,
    }
}

/// BuildCastFunction4Union build a implicitly CAST ScalarFunction from the Union Expression.
/// Go logs the error and returns the node with a nil signature; this returns the error.
// go: BuildCastFunction4Union L2 (returns the error Go logs)
pub fn build_cast_function4_union(
    ctx: &dyn BuildContext,
    expr: ExprRef,
    tp: FieldType,
) -> Result<ExprRef, Error> {
    build_cast_function_with_check(ctx, expr, tp, true, false)
}

/// BuildCastFunction builds a CAST ScalarFunction from the Expression. Go logs the error
/// and returns the node with a nil signature; this returns the error.
// go: BuildCastFunction L2 (returns the error Go logs)
pub fn build_cast_function(
    ctx: &dyn BuildContext,
    expr: ExprRef,
    tp: FieldType,
) -> Result<ExprRef, Error> {
    build_cast_function_with_check(ctx, expr, tp, false, false)
}

/// The CAST targets of D08.8 part 2: a temporary stub (constitution art. 4.3).
// glue: temporary stub of the castAsXxxFunctionClass not ported yet (D08.8 part 2)
fn cast_not_ported(et: EvalType) -> Error {
    Error::plain(format!(
        "tidb-rs: CAST to {} is not ported yet (D08.8 part 2)",
        et.to_string()
    ))
}

/// BuildCastFunctionWithCheck builds a CAST ScalarFunction from the Expression and return
/// error if any. Go returns the node with a nil signature together with an error; this
/// returns only the error.
// go: BuildCastFunctionWithCheck L2 (the error comes without the node; CAST to STRING/TIME/DURATION/JSON/VECTOR is a temporary stub)
pub fn build_cast_function_with_check(
    ctx: &dyn BuildContext,
    expr: ExprRef,
    mut tp: FieldType,
    in_union: bool,
    _is_explicit_charset: bool,
) -> Result<ExprRef, Error> {
    let ectx = ctx.get_eval_ctx();
    // If source argument's nullable, then target type should be nullable
    if !has_not_null_flag(expr.get_type(&*ectx).get_flag()) {
        tp.del_flag(NotNullFlag);
    }
    let expr = try_push_cast_into_control_function_for_hybrid_type(ctx, expr, &tp);
    let base = BaseFunctionClass {
        func_name: ast::Cast,
        min_args: 1,
        max_args: 1,
    };
    let et = tp.eval_type();
    let f = match et {
        ETInt => CastAsIntFunctionClass {
            base,
            tp,
            in_union,
        }
        .get_function(ctx, vec![expr])?,
        ETDecimal => CastAsDecimalFunctionClass {
            base,
            tp,
            in_union,
        }
        .get_function(ctx, vec![expr])?,
        ETReal => CastAsRealFunctionClass {
            base,
            tp,
            in_union,
        }
        .get_function(ctx, vec![expr])?,
        ETDatetime | ETTimestamp | ETDuration | ETJson | ETVectorFloat32 | ETString => {
            return Err(cast_not_ported(et));
        }
        _ => return Err(Error::plain(format!("cannot cast from {}", et.to_string()))),
    };
    // Go's RetType is the class's tp, the pointer the signature keeps (newBaseBuiltinFunc
    // may have changed its NOT NULL flag).
    let res = Expr::ScalarFunction(ScalarFunction::new(ast::Cast, f.tp.clone(), f)).into_ref();
    // We do not fold CAST if the eval type of this scalar function is ETJson since we may
    // reset the flag of the field type of CastAsJson later which would affect the
    // evaluation of it.
    if et != ETJson {
        return Ok(fold_constant(ctx, res));
    }
    Ok(res)
}

/// The control functions TryPushCastIntoControlFunctionForHybridType rebuilds (IF, CASE,
/// ELT) are not ported yet, so no node built by tidb-rs can be one of them: the function
/// returns `expr` (temporary stub, constitution art. 4.3; rebuilt with D08.9).
// go: TryPushCastIntoControlFunctionForHybridType L2 (temporary stub: IF/CASE/ELT are not ported, D08.9)
pub fn try_push_cast_into_control_function_for_hybrid_type(
    ctx: &dyn BuildContext,
    expr: ExprRef,
    tp: &FieldType,
) -> ExprRef {
    let Some(sf) = expr.as_scalar_function() else {
        return expr;
    };
    if !matches!(tp.eval_type(), ETInt | ETReal) {
        return expr;
    }
    let ectx = ctx.get_eval_ctx();
    let is_hybrid = |e: &ExprRef| {
        let ft = e.get_type(&*ectx);
        // todo: compatible with mysql control function using bit type. issue 24725
        ft.hybrid() && ft.get_type() != mysql::TypeBit
    };
    let args = sf.get_args();
    let needs = match sf.func_name.l.as_str() {
        ast::If => is_hybrid(&args[1]) || is_hybrid(&args[2]),
        ast::Case => {
            let mut has = false;
            let mut i = 0;
            while i + 1 < args.len() {
                has = has || is_hybrid(&args[i + 1]);
                i += 2;
            }
            has || (args.len() % 2 == 1 && is_hybrid(&args[args.len() - 1]))
        }
        ast::Elt => args[1..].iter().any(is_hybrid),
        _ => false,
    };
    if needs {
        unimplemented!(
            "TryPushCastIntoControlFunctionForHybridType: {} is not ported yet (D08.9)",
            sf.func_name.l
        );
    }
    expr
}

/// WrapWithCastAsInt wraps `expr` with `cast` if the return type of expr is not type int,
/// otherwise, returns `expr` directly.
// go: WrapWithCastAsInt L2 (returns the error BuildCastFunction logs)
pub fn wrap_with_cast_as_int(ctx: &dyn BuildContext, mut expr: ExprRef) -> Result<ExprRef, Error> {
    let ectx = ctx.get_eval_ctx();
    if expr.get_type(&*ectx).get_type() == TypeEnum {
        // Go clones a Column and its RetType first; any other node is changed in place.
        with_type_mut(&mut expr, |t| t.add_flag(EnumSetAsIntFlag));
    }
    let et = expr.get_type(&*ectx).into_owned();
    if et.eval_type() == ETInt {
        return Ok(expr);
    }
    let mut tp = new_field_type(TypeLonglong);
    tp.set_flen(et.get_flen());
    tp.set_decimal(0);
    set_bin_chs_cln_flag(&mut tp);
    tp.add_flag(et.get_flag() & (UnsignedFlag | NotNullFlag));
    build_cast_function(ctx, expr, tp)
}

/// WrapWithCastAsReal wraps `expr` with `cast` if the return type of expr is not type real,
/// otherwise, returns `expr` directly.
// go: WrapWithCastAsReal L2 (returns the error BuildCastFunction logs)
pub fn wrap_with_cast_as_real(ctx: &dyn BuildContext, expr: ExprRef) -> Result<ExprRef, Error> {
    let ectx = ctx.get_eval_ctx();
    let et = expr.get_type(&*ectx).into_owned();
    if et.eval_type() == ETReal {
        return Ok(expr);
    }
    let mut tp = new_field_type(TypeDouble);
    tp.set_flen(MaxRealWidth);
    tp.set_decimal(UnspecifiedLength);
    set_bin_chs_cln_flag(&mut tp);
    tp.add_flag(et.get_flag() & (UnsignedFlag | NotNullFlag));
    build_cast_function(ctx, expr, tp)
}

// go: minimalDecimalLenForHoldingInteger L1
fn minimal_decimal_len_for_holding_integer(tp: u8) -> isize {
    match tp {
        TypeTiny => 3,
        TypeShort => 5,
        TypeInt24 => 8,
        TypeLong => 10,
        TypeLonglong => 20,
        TypeYear => 4,
        _ => MaxIntWidth,
    }
}

/// WrapWithCastAsDecimal wraps `expr` with `cast` if the return type of expr is not type
/// decimal, otherwise, returns `expr` directly.
// go: WrapWithCastAsDecimal L2 (returns the error BuildCastFunction logs)
#[allow(clippy::collapsible_if)]
pub fn wrap_with_cast_as_decimal(
    ctx: &dyn BuildContext,
    expr: ExprRef,
) -> Result<ExprRef, Error> {
    let ectx = ctx.get_eval_ctx();
    let et = expr.get_type(&*ectx).into_owned();
    if et.eval_type() == ETDecimal {
        return Ok(expr);
    }
    let mut tp = new_field_type(TypeNewDecimal);
    tp.set_flen_under_limit(et.get_flen());
    tp.set_decimal_under_limit(et.get_decimal());

    if et.eval_type() == ETInt {
        tp.set_flen(minimal_decimal_len_for_holding_integer(et.get_type()));
        tp.set_decimal(0);
    }
    if tp.get_flen() == UnspecifiedLength || tp.get_flen() > MaxDecimalWidth {
        tp.set_flen(MaxDecimalWidth);
    }
    set_bin_chs_cln_flag(&mut tp);
    tp.add_flag(et.get_flag() & (UnsignedFlag | NotNullFlag));
    let mut cast_expr = build_cast_function(ctx, expr, tp)?;
    // For const item, we can use find-grained precision and scale by the result.
    if cast_expr.const_level() == ConstStrict {
        if let Ok(Some(val)) =
            cast_expr.eval_decimal(&*ectx, &EvalState::new(), tidb_chunk::Row::default())
        {
            let (precision, frac) = val.precision_and_frac();
            with_type_mut(&mut cast_expr, |t| {
                t.set_decimal_under_limit(frac);
                t.set_flen_under_limit(precision);
            });
        }
    }
    Ok(cast_expr)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn float_length_matches_go() {
        assert_eq!(set_data_type_double(2), (23 + 2 - 8 + NotFixedDec, NotFixedDec));
        assert_eq!(float_length(NotFixedDec, 0), 23);
    }
}
