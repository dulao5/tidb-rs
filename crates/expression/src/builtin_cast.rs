//! go-file: pkg/expression/builtin_cast.go
//! go-file: pkg/expression/builtin_cast_vec.go
//!
//! CAST (block D08.8): the function classes of CAST to INT, REAL, DECIMAL and STRING, their
//! signatures from INT, REAL, DECIMAL and STRING (and hybrid) arguments, `BuildCastFunction*`,
//! `BuildCastCollationFunction` and `WrapWithCastAsInt`/`Real`/`Decimal`/`String` (spec D08.8
//! §1).
//!
//! The signatures from TIME, DURATION, JSON and VECTOR arguments to INT, REAL, DECIMAL and
//! STRING are ported (part 2b-2a), CAST to DATETIME/DATE/TIMESTAMP and TIME (duration) with
//! `builtinCastVectorFloat32AsUnsupportedSig` (part 2b-2b-i), and CAST to JSON, JSON ARRAY
//! and VECTOR with `WrapWithCastAsJSON`/`VectorFloat32` and `ConvertJSON2Tp` (part 2b-2b-ii).

#![allow(non_upper_case_globals)]

use std::sync::Arc;

use tidb_base::Error;
use tidb_base::errors::RedactLogDisable;
use tidb_base::gofmt::Arg;
use tidb_base::gostd::bytes::trim_space;
use tidb_base::gostd::strconv::format_float;
use tidb_base::gostd::time as gotime;
use tidb_base::mysql::{
    self, BinaryFlag, EnumSetAsIntFlag, MaxDateWidth, MaxDatetimeWidthNoFsp, MaxDecimalScale,
    MaxDecimalWidth, MaxDurationWidthNoFsp, MaxIntWidth, MaxLongBlobWidth, MaxRealWidth,
    NotFixedDec, NotNullFlag, TypeBit, TypeBlob, TypeDate, TypeDatetime, TypeDouble, TypeDuration,
    TypeEnum, TypeFloat, TypeInt24, TypeLong, TypeLongBlob, TypeLonglong, TypeMediumBlob,
    TypeNewDecimal, TypeNull, TypeSet, TypeShort, TypeString, TypeTimestamp, TypeTiny,
    TypeTinyBlob, TypeVarString, TypeVarchar, TypeYear, UnsignedFlag, has_not_null_flag,
    has_unsigned_flag,
};
use tidb_base::mysql::{
    DefaultCharset, DefaultCollationName, ParseToJSONFlag, TypeJSON, TypeTiDBVectorFloat32,
    has_is_boolean_flag, has_parse_to_json_flag,
};
use tidb_parser::gocharset::{CharsetBin, CharsetUTF8MB4, CollationBin};
use tidb_parser::types::type_str;
use tidb_parser::types::{
    ETDatetime, ETDecimal, ETDuration, ETInt, ETJson, ETReal, ETString, ETTimestamp,
    ETVectorFloat32, EvalType, UnspecifiedLength,
};
use tidb_types::context::{Context as TypeContext, StrictContext};
use tidb_types::convert::number_to_duration;
use tidb_types::convert::{
    convert_float_to_int, convert_float_to_uint, convert_json_to_decimal, convert_json_to_float,
    convert_json_to_int, convert_json_to_int64, integer_signed_lower_bound,
    integer_signed_upper_bound, integer_unsigned_upper_bound, str_to_float, str_to_int,
    str_to_uint,
};
use tidb_types::datum::{
    produce_dec_with_specified_tp, produce_float_with_specified_tp, produce_str_with_specified_tp,
};
use tidb_types::errors::{
    DateTimeStr, ErrCastAsSignedOverflow, ErrCastNegIntAsUnsigned, ErrIncorrectDatetimeValue,
    ErrOverflow, ErrTruncated, ErrTruncatedWrongVal, ErrWrongValue,
};
use tidb_types::etc::{FieldType, is_binary_str};
use tidb_types::field_type::{new_field_type, set_bin_chs_cln_flag};
use tidb_types::fsp::{DefaultFsp, MaxFsp, MinFsp, UnspecifiedFsp, check_fsp};
use tidb_types::json::{
    BinaryJSON, JSONTypeCodeArray, JSONTypeCodeDate, JSONTypeCodeDatetime, JSONTypeCodeDuration,
    JSONTypeCodeFloat64, JSONTypeCodeInt64, JSONTypeCodeObject, JSONTypeCodeString,
    JSONTypeCodeTimestamp, JSONTypeCodeUint64, JSONValue, Opaque, create_binary_json,
    parse_binary_json_from_string,
};
use tidb_types::mydecimal::{MODE_HALF_UP, MyDecimal, new_dec_from_int, new_dec_from_uint};
use tidb_types::time::{
    Duration, Time, ZeroTime, from_date, parse_duration, parse_time, parse_time_from_float_string,
    parse_time_from_num, parse_time_from_year,
};
use tidb_types::vector::{VectorFloat32, parse_vector_float32};

use crate::ast_funcs as ast;
use crate::builtin::{
    BaseFunctionClass, new_base_builtin_cast_func, new_base_builtin_cast_func4_string,
    new_base_builtin_func,
};
use crate::builtin_convert_charset::handle_binary_literal;
use crate::collation::{CoercibilityExplicit, ExprCollation};
use crate::constant_fold::fold_constant;
use crate::context::{BuildContext, EvalContext, err_ctx, location, sql_mode, type_ctx};
use crate::errors::{ErrInvalidJSONForFuncIndex, ErrInvalidTypeForJSON, ErrNotSupportedYet};
use crate::errors::{handle_allowed_packet_overflowed, handle_invalid_time_error};
use crate::eval::{Args, BuiltinFunc, SigDesc, SigDrivers, SigView};
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
const CastIntAsString: i32 = 3;
const CastRealAsString: i32 = 12;
const CastDecimalAsString: i32 = 22;
const CastStringAsString: i32 = 32;
const CastTimeAsString: i32 = 42;
const CastDurationAsString: i32 = 52;
const CastJsonAsString: i32 = 62;
const CastVectorFloat32AsString: i32 = 5183;
const CastIntAsTime: i32 = 5;
const CastIntAsDuration: i32 = 6;
const CastRealAsTime: i32 = 14;
const CastRealAsDuration: i32 = 15;
const CastDecimalAsTime: i32 = 24;
const CastDecimalAsDuration: i32 = 25;
const CastStringAsTime: i32 = 34;
const CastStringAsDuration: i32 = 35;
const CastTimeAsTime: i32 = 44;
const CastTimeAsDuration: i32 = 45;
const CastDurationAsTime: i32 = 54;
const CastDurationAsDuration: i32 = 55;
const CastJsonAsTime: i32 = 64;
const CastJsonAsDuration: i32 = 65;
const CastIntAsJson: i32 = 7;
const CastRealAsJson: i32 = 16;
const CastDecimalAsJson: i32 = 26;
const CastStringAsJson: i32 = 36;
const CastTimeAsJson: i32 = 46;
const CastDurationAsJson: i32 = 56;
const CastJsonAsJson: i32 = 66;
const CastVectorFloat32AsVectorFloat32: i32 = 5188;

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
sig!(
    builtinCastIntAsRealSig,
    CastIntAsReal,
    true,
    CAST_INT_AS_REAL
);
sig!(
    builtinCastIntAsDecimalSig,
    CastIntAsDecimal,
    true,
    CAST_INT_AS_DECIMAL
);
sig!(
    builtinCastRealAsIntSig,
    CastRealAsInt,
    true,
    CAST_REAL_AS_INT
);
sig!(
    builtinCastRealAsRealSig,
    CastRealAsReal,
    true,
    CAST_REAL_AS_REAL
);
sig!(
    builtinCastRealAsDecimalSig,
    CastRealAsDecimal,
    true,
    CAST_REAL_AS_DECIMAL
);
sig!(
    builtinCastDecimalAsIntSig,
    CastDecimalAsInt,
    true,
    CAST_DECIMAL_AS_INT
);
sig!(
    builtinCastDecimalAsRealSig,
    CastDecimalAsReal,
    true,
    CAST_DECIMAL_AS_REAL
);
sig!(
    builtinCastDecimalAsDecimalSig,
    CastDecimalAsDecimal,
    true,
    CAST_DECIMAL_AS_DECIMAL
);
sig!(
    builtinCastStringAsIntSig,
    CastStringAsInt,
    true,
    CAST_STRING_AS_INT
);
sig!(
    builtinCastStringAsRealSig,
    CastStringAsReal,
    true,
    CAST_STRING_AS_REAL
);
sig!(
    builtinCastStringAsDecimalSig,
    CastStringAsDecimal,
    true,
    CAST_STRING_AS_DECIMAL
);

sig!(
    builtinCastTimeAsIntSig,
    CastTimeAsInt,
    true,
    CAST_TIME_AS_INT
);
sig!(
    builtinCastTimeAsRealSig,
    CastTimeAsReal,
    true,
    CAST_TIME_AS_REAL
);
sig!(
    builtinCastTimeAsDecimalSig,
    CastTimeAsDecimal,
    true,
    CAST_TIME_AS_DECIMAL
);
sig!(
    builtinCastDurationAsIntSig,
    CastDurationAsInt,
    true,
    CAST_DURATION_AS_INT
);
sig!(
    builtinCastDurationAsRealSig,
    CastDurationAsReal,
    true,
    CAST_DURATION_AS_REAL
);
sig!(
    builtinCastDurationAsDecimalSig,
    CastDurationAsDecimal,
    true,
    CAST_DURATION_AS_DECIMAL
);
sig!(
    builtinCastJSONAsIntSig,
    CastJsonAsInt,
    true,
    CAST_JSON_AS_INT
);
sig!(
    builtinCastJSONAsRealSig,
    CastJsonAsReal,
    true,
    CAST_JSON_AS_REAL
);
sig!(
    builtinCastJSONAsDecimalSig,
    CastJsonAsDecimal,
    true,
    CAST_JSON_AS_DECIMAL
);
sig!(
    builtinCastIntAsStringSig,
    CastIntAsString,
    true,
    CAST_INT_AS_STRING
);
sig!(
    builtinCastRealAsStringSig,
    CastRealAsString,
    true,
    CAST_REAL_AS_STRING
);
sig!(
    builtinCastDecimalAsStringSig,
    CastDecimalAsString,
    true,
    CAST_DECIMAL_AS_STRING
);
sig!(
    builtinCastStringAsStringSig,
    CastStringAsString,
    true,
    CAST_STRING_AS_STRING
);
sig!(
    builtinCastTimeAsStringSig,
    CastTimeAsString,
    true,
    CAST_TIME_AS_STRING
);
sig!(
    builtinCastDurationAsStringSig,
    CastDurationAsString,
    true,
    CAST_DURATION_AS_STRING
);
sig!(
    builtinCastJSONAsStringSig,
    CastJsonAsString,
    true,
    CAST_JSON_AS_STRING
);
sig!(
    builtinCastVectorFloat32AsStringSig,
    CastVectorFloat32AsString,
    false,
    CAST_VECTOR_FLOAT32_AS_STRING
);
// Go does not call setPbCode for it (the code is commented out).
sig!(
    builtinCastVectorFloat32AsUnsupportedSig,
    0,
    false,
    SigDrivers::Unsupported {
        err: cast_vector_float32_as_unsupported
    }
);
sig!(
    builtinCastIntAsJSONSig,
    CastIntAsJson,
    true,
    CAST_INT_AS_JSON
);
sig!(
    builtinCastRealAsJSONSig,
    CastRealAsJson,
    true,
    CAST_REAL_AS_JSON
);
sig!(
    builtinCastDecimalAsJSONSig,
    CastDecimalAsJson,
    true,
    CAST_DECIMAL_AS_JSON
);
sig!(
    builtinCastStringAsJSONSig,
    CastStringAsJson,
    true,
    CAST_STRING_AS_JSON
);
sig!(
    builtinCastTimeAsJSONSig,
    CastTimeAsJson,
    true,
    CAST_TIME_AS_JSON
);
sig!(
    builtinCastDurationAsJSONSig,
    CastDurationAsJson,
    true,
    CAST_DURATION_AS_JSON
);
sig!(
    builtinCastJSONAsJSONSig,
    CastJsonAsJson,
    true,
    CAST_JSON_AS_JSON
);
// Go does not call setPbCode for it.
sig!(castJSONAsArrayFunctionSig, 0, false, CAST_JSON_AS_ARRAY);
// Go does not call setPbCode for these two (the code is commented out).
sig!(
    builtinCastUnsupportedAsVectorFloat32Sig,
    0,
    false,
    CAST_UNSUPPORTED_AS_VECTOR_FLOAT32
);
sig!(
    builtinCastStringAsVectorFloat32Sig,
    0,
    false,
    CAST_STRING_AS_VECTOR_FLOAT32
);
sig!(
    builtinCastVectorFloat32AsVectorFloat32Sig,
    CastVectorFloat32AsVectorFloat32,
    false,
    CAST_VECTOR_FLOAT32_AS_VECTOR_FLOAT32
);
sig!(
    builtinCastIntAsTimeSig,
    CastIntAsTime,
    true,
    CAST_INT_AS_TIME
);
sig!(
    builtinCastRealAsTimeSig,
    CastRealAsTime,
    true,
    CAST_REAL_AS_TIME
);
sig!(
    builtinCastDecimalAsTimeSig,
    CastDecimalAsTime,
    true,
    CAST_DECIMAL_AS_TIME
);
sig!(
    builtinCastStringAsTimeSig,
    CastStringAsTime,
    true,
    CAST_STRING_AS_TIME
);
sig!(
    builtinCastTimeAsTimeSig,
    CastTimeAsTime,
    true,
    CAST_TIME_AS_TIME
);
sig!(
    builtinCastDurationAsTimeSig,
    CastDurationAsTime,
    true,
    CAST_DURATION_AS_TIME
);
sig!(
    builtinCastJSONAsTimeSig,
    CastJsonAsTime,
    true,
    CAST_JSON_AS_TIME
);
sig!(
    builtinCastIntAsDurationSig,
    CastIntAsDuration,
    true,
    CAST_INT_AS_DURATION
);
sig!(
    builtinCastRealAsDurationSig,
    CastRealAsDuration,
    true,
    CAST_REAL_AS_DURATION
);
sig!(
    builtinCastDecimalAsDurationSig,
    CastDecimalAsDuration,
    true,
    CAST_DECIMAL_AS_DURATION
);
sig!(
    builtinCastStringAsDurationSig,
    CastStringAsDuration,
    true,
    CAST_STRING_AS_DURATION
);
sig!(
    builtinCastTimeAsDurationSig,
    CastTimeAsDuration,
    true,
    CAST_TIME_AS_DURATION
);
sig!(
    builtinCastDurationAsDurationSig,
    CastDurationAsDuration,
    true,
    CAST_DURATION_AS_DURATION
);
sig!(
    builtinCastJSONAsDurationSig,
    CastJsonAsDuration,
    true,
    CAST_JSON_AS_DURATION
);

/// `b.inUnion` of a `baseBuiltinCastFunc`.
#[inline(always)]
// glue: read of the embedded baseBuiltinCastFunc.inUnion
fn in_union(f: &BuiltinFunc) -> bool {
    f.in_union() == Some(true)
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
    Ok(Some(
        res.expect("nil *MyDecimal from ProduceDecWithSpecifiedTp"),
    ))
}

/// `builtinCastIntAsIntSig.evalInt`.
// go: builtinCastIntAsIntSig.evalInt L1
// go: builtinCastIntAsIntSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinCastIntAsIntSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
// go: builtinCastIntAsIntSig.vectorized L2 (SigDesc.vectorized)
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
// go: builtinCastIntAsRealSig.vecEvalReal L2 (the vector driver of this body, M-EVAL)
// go: builtinCastIntAsRealSig.vectorized L2 (SigDesc.vectorized)
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
// go: builtinCastIntAsDecimalSig.vecEvalDecimal L2 (the vector driver of this body, M-EVAL)
// go: builtinCastIntAsDecimalSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinCastIntAsDecimalSig.vectorized L2 (SigDesc.vectorized)
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
        // The vector version writes dec.FromUint(0), the row version &types.MyDecimal{}.
        if f.vector() {
            new_dec_from_uint(0)
        } else {
            MyDecimal::default()
        }
    } else {
        new_dec_from_uint(val as u64)
    };
    produce_dec(ctx, f, res)
}

// go: builtinCastRealAsRealSig.evalReal L1
// go: builtinCastRealAsRealSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinCastRealAsRealSig.vecEvalReal L2 (the vector driver of this body, M-EVAL)
// go: builtinCastRealAsRealSig.vectorized L2 (SigDesc.vectorized)
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
// go: builtinCastRealAsIntSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
// go: builtinCastRealAsIntSig.vectorized L2 (SigDesc.vectorized)
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
// go: builtinCastRealAsDecimalSig.vecEvalDecimal L2 (the vector driver of this body, M-EVAL)
// go: builtinCastRealAsDecimalSig.vectorized L2 (SigDesc.vectorized)
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
// go: builtinCastDecimalAsDecimalSig.vecEvalDecimal L2 (the vector driver of this body, M-EVAL)
// go: builtinCastDecimalAsDecimalSig.vectorized L2 (SigDesc.vectorized)
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
// go: builtinCastDecimalAsIntSig.vecEvalInt L2 (the vector driver of this body; its warning text differs from the row version's, as in Go)
// go: builtinCastDecimalAsIntSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinCastDecimalAsIntSig.vectorized L2 (SigDesc.vectorized)
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
            // The row version passes the *MyDecimal (printed by its String method); the
            // vector version passes the types.MyDecimal value, which fmt prints field by
            // field (`{%!s(int8=..) ..}`): each driver keeps its Go text (spec §4).
            let shown = if f.vector() {
                decimal_value_arg(val)
            } else {
                Arg::Stringer(val.string().into_bytes(), "*types.MyDecimal")
            };
            let warn_err =
                ErrTruncatedWrongVal.gen_with_stack_by_args(vec![Arg::from("DECIMAL"), shown]);
            err_ctx(ctx).handle_error_with_alias(e.clone(), e, warn_err)?;
        }
        other => other?,
    }
    Ok(Some(res))
}

// go: builtinCastDecimalAsRealSig.evalReal L1
// go: builtinCastDecimalAsRealSig.vecEvalReal L2 (the vector driver of this body, M-EVAL)
// go: builtinCastDecimalAsRealSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinCastDecimalAsRealSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Real)]
pub fn cast_decimal_as_real(
    ctx: &dyn EvalContext,
    f: &impl SigView,
    val: &MyDecimal,
) -> Result<Option<f64>, Error> {
    if in_union(f) && has_unsigned_flag(f.tp.get_flag()) && val.is_negative() {
        return Ok(Some(0.0));
    }
    let (res, err) = val.to_float64();
    match err {
        // Only the vector version hands an overflow to ec.HandleError.
        Err(e) if f.vector() && ErrOverflow.equal(&e) => err_ctx(ctx).handle_error(e)?,
        other => other?,
    }
    Ok(Some(res))
}

/// A `types.MyDecimal` value as a `fmt` argument: printed by reflection (its `String` method
/// has a pointer receiver), fields `digitsInt digitsFrac resultFrac negative wordBuf`.
// glue: Go passes a types.MyDecimal value to GenWithStackByArgs (builtinCastDecimalAsIntSig.vecEvalInt)
pub(crate) fn decimal_value_arg(d: &MyDecimal) -> Arg {
    let w = d.as_wire();
    let words = (0..9)
        .map(|i| {
            let at = 4 + 4 * i;
            Arg::Int(
                i32::from_le_bytes([w[at], w[at + 1], w[at + 2], w[at + 3]]) as i64,
                "int32",
            )
        })
        .collect();
    Arg::Struct(
        vec![
            Arg::Int(w[0] as i8 as i64, "int8"),
            Arg::Int(w[1] as i8 as i64, "int8"),
            Arg::Int(w[2] as i8 as i64, "int8"),
            Arg::Bool(w[3] != 0),
            Arg::Array(words, "[9]int32"),
        ],
        "types.MyDecimal",
    )
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
        let res = if is_negative {
            i64::MIN
        } else {
            u64::MAX as i64
        };
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
// go: builtinCastStringAsIntSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
// go: builtinCastStringAsIntSig.vectorized L2 (SigDesc.vectorized)
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
// go: builtinCastStringAsRealSig.vecEvalReal L2 (the vector driver of this body, M-EVAL)
// go: builtinCastStringAsRealSig.vectorized L2 (SigDesc.vectorized)
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

/// Go's vector version differs from the row version in what the user sees: `-` alone counts
/// as negative, the `FromString` error goes to `tc.HandleTruncate` as is (no
/// `ErrTruncatedWrongVal` text, type flags instead of error levels), and a negative value in
/// UNSIGNED UNION leaves the row as `ResizeDecimal` left it (zero in a new column). The vector
/// driver follows it (spec §4; D08.1 spec §3.4 (c)).
// go: builtinCastStringAsDecimalSig.evalDecimal L2 (the binary-literal prologue is unreachable from castAsDecimalFunctionClass)
// go: builtinCastStringAsDecimalSig.vecEvalDecimal L2 (an UNSIGNED UNION negative row is the zero decimal, i.e. a new column's bytes)
// go: builtinCastStringAsDecimalSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinCastStringAsDecimalSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Decimal)]
pub fn cast_string_as_decimal(
    ctx: &dyn EvalContext,
    f: &impl SigView,
    val: &[u8],
) -> Result<Option<MyDecimal>, Error> {
    let val = trim_space(val);
    if f.vector() {
        let is_negative = !val.is_empty() && val[0] == b'-';
        let mut dec = MyDecimal::default();
        if !(in_union(f) && has_unsigned_flag(f.tp.get_flag()) && is_negative) {
            let tc = type_ctx(ctx);
            tc.handle_truncate(dec.from_string(val))?;
            let (dec, err) = produce_dec_with_specified_tp(&tc, dec, &f.tp);
            if let Err(e) = err {
                err_ctx(ctx).handle_error(e)?;
            }
            return Ok(Some(
                dec.expect("nil *MyDecimal from ProduceDecWithSpecifiedTp"),
            ));
        }
        return Ok(Some(dec));
    }
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

// ---------------------------------------------------------------- CAST to STRING

/// padZeroForBinaryType pads `s` with `\0` up to the length of a `BINARY(n)` result. When the
/// length exceeds `max_allowed_packet`, the result is NULL with a warning (or an error,
/// `handleAllowedPacketOverflowed`).
// go: padZeroForBinaryType L1
fn pad_zero_for_binary_type(
    ctx: &dyn EvalContext,
    mut s: Vec<u8>,
    tp: &FieldType,
    out: &mut Vec<u8>,
) -> Result<Option<()>, Error> {
    let flen = tp.get_flen();
    if tp.get_type() == TypeString && is_binary_str(tp) && (s.len() as isize) < flen {
        let max_allowed_packet = ctx.get_max_allowed_packet();
        if flen as u64 > max_allowed_packet {
            handle_allowed_packet_overflowed(ctx, "cast_as_binary", max_allowed_packet)?;
            return Ok(None);
        }
        s.resize(flen as usize, 0);
    }
    out.extend_from_slice(&s);
    Ok(Some(()))
}

/// The shared tail of the CAST-to-STRING signatures: `ProduceStrWithSpecifiedTp` (its error
/// is returned) then `padZeroForBinaryType`.
// glue: the shared tail `res, err = ProduceStrWithSpecifiedTp(..); if err != nil {..}; return padZeroForBinaryType(..)` of the CAST-to-STRING signatures
fn produce_str(
    ctx: &dyn EvalContext,
    f: &BuiltinFunc,
    res: Vec<u8>,
    out: &mut Vec<u8>,
) -> Result<Option<()>, Error> {
    let (res, err) = produce_str_with_specified_tp(res, &f.tp, &type_ctx(ctx), false);
    err?;
    pad_zero_for_binary_type(ctx, res, &f.tp, out)
}

// go: builtinCastIntAsStringSig.evalString L1
// go: builtinCastIntAsStringSig.vecEvalString L2 (the vector driver of this body, M-EVAL)
// go: builtinCastIntAsStringSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinCastIntAsStringSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(String)]
pub fn cast_int_as_string(
    ctx: &dyn EvalContext,
    f: &impl SigView,
    val: i64,
    out: &mut Vec<u8>,
) -> Result<Option<()>, Error> {
    let tp = f.arg_type(0);
    let mut res = if !has_unsigned_flag(tp.get_flag()) {
        val.to_string()
    } else {
        (val as u64).to_string()
    }
    .into_bytes();
    if tp.get_type() == TypeYear && res == b"0" {
        res = b"0000".to_vec();
    }
    produce_str(ctx, f, res, out)
}

// go: builtinCastRealAsStringSig.evalString L1
// go: builtinCastRealAsStringSig.vecEvalString L2 (the vector driver of this body, M-EVAL)
// go: builtinCastRealAsStringSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinCastRealAsStringSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(String)]
pub fn cast_real_as_string(
    ctx: &dyn EvalContext,
    f: &impl SigView,
    val: f64,
    out: &mut Vec<u8>,
) -> Result<Option<()>, Error> {
    let mut bits = 64;
    if f.arg_type(0).get_type() == TypeFloat {
        // b.args[0].EvalReal() casts the value from float32 to float64, for example:
        // float32(208.867) is cast to float64(208.86700439)
        // If we strconv.FormatFloat the value with 64bits, the result is incorrect!
        bits = 32;
    }
    produce_str(ctx, f, format_float(val, b'f', -1, bits).into_bytes(), out)
}

// go: builtinCastDecimalAsStringSig.evalString L1
// go: builtinCastDecimalAsStringSig.vecEvalString L2 (the vector driver of this body, M-EVAL)
// go: builtinCastDecimalAsStringSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinCastDecimalAsStringSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(String)]
pub fn cast_decimal_as_string(
    ctx: &dyn EvalContext,
    f: &impl SigView,
    val: &MyDecimal,
    out: &mut Vec<u8>,
) -> Result<Option<()>, Error> {
    produce_str(ctx, f, val.to_string(), out)
}

// go: builtinCastStringAsStringSig.evalString L1
// go: builtinCastStringAsStringSig.vecEvalString L2 (the vector driver of this body, M-EVAL)
// go: builtinCastStringAsStringSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinCastStringAsStringSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(String)]
pub fn cast_string_as_string(
    ctx: &dyn EvalContext,
    f: &impl SigView,
    val: &[u8],
    out: &mut Vec<u8>,
) -> Result<Option<()>, Error> {
    produce_str(ctx, f, val.to_vec(), out)
}

// ---------------------------------------------------------------- from TIME, DURATION, JSON, VECTOR

/// A `fsp` argument of `Duration.ToNumber` checked by `types.CheckFsp`.
// glue: the shared `val.Fsp, err = types.CheckFsp(val.Fsp)` of the duration signatures
fn checked_fsp(mut d: Duration) -> Result<Duration, Error> {
    let (fsp, err) = check_fsp(d.fsp);
    d.fsp = fsp;
    err?;
    Ok(d)
}

// go: builtinCastTimeAsIntSig.evalInt L1
// go: builtinCastTimeAsIntSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
// go: builtinCastTimeAsIntSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinCastTimeAsIntSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Int)]
pub fn cast_time_as_int(
    ctx: &dyn EvalContext,
    _f: &impl SigView,
    val: Time,
) -> Result<Option<i64>, Error> {
    let (t, err) = val.round_frac(&type_ctx(ctx), DefaultFsp);
    err?;
    let (res, err) = t.to_number().to_int();
    err?;
    Ok(Some(res))
}

// go: builtinCastTimeAsRealSig.evalReal L1
// go: builtinCastTimeAsRealSig.vecEvalReal L2 (the vector driver of this body; only it hands ErrOverflow to ec.HandleError and sets the row NULL, as in Go)
// go: builtinCastTimeAsRealSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinCastTimeAsRealSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Real)]
pub fn cast_time_as_real(
    ctx: &dyn EvalContext,
    f: &impl SigView,
    val: Time,
) -> Result<Option<f64>, Error> {
    let (res, err) = val.to_number().to_float64();
    match err {
        Err(e) if f.vector() && ErrOverflow.equal(&e) => {
            err_ctx(ctx).handle_error(e)?;
            Ok(None)
        }
        other => {
            other?;
            Ok(Some(res))
        }
    }
}

// go: builtinCastTimeAsDecimalSig.evalDecimal L1
// go: builtinCastTimeAsDecimalSig.vecEvalDecimal L2 (the vector driver of this body; FillNumber = ToNumber, M-EVAL)
// go: builtinCastTimeAsDecimalSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinCastTimeAsDecimalSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Decimal)]
pub fn cast_time_as_decimal(
    ctx: &dyn EvalContext,
    f: &impl SigView,
    val: Time,
) -> Result<Option<MyDecimal>, Error> {
    produce_dec(ctx, f, val.to_number())
}

// go: builtinCastTimeAsStringSig.evalString L1
// go: builtinCastTimeAsStringSig.vecEvalString L2 (the vector driver of this body, M-EVAL)
// go: builtinCastTimeAsStringSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinCastTimeAsStringSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(String)]
pub fn cast_time_as_string(
    ctx: &dyn EvalContext,
    f: &impl SigView,
    val: Time,
    out: &mut Vec<u8>,
) -> Result<Option<()>, Error> {
    produce_str(ctx, f, val.string(), out)
}

// go: builtinCastDurationAsIntSig.evalInt L1
// go: builtinCastDurationAsIntSig.vecEvalInt L2 (the vector driver of this body; it reads the durations with the argument's declared fsp, M-EVAL)
// go: builtinCastDurationAsIntSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinCastDurationAsIntSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Int)]
pub fn cast_duration_as_int(
    ctx: &dyn EvalContext,
    f: &impl SigView,
    val: Duration,
) -> Result<Option<i64>, Error> {
    let (res, err) = if f.tp.get_type() == TypeYear {
        val.convert_to_year(&type_ctx(ctx))
    } else {
        let (dur, err) = val.round_frac(DefaultFsp, &location(ctx));
        err?;
        dur.to_number().to_int()
    };
    err?;
    Ok(Some(res))
}

// go: builtinCastDurationAsRealSig.evalReal L1
// go: builtinCastDurationAsRealSig.vecEvalReal L2 (the vector driver of this body; Go checks the argument's declared fsp once before the loop, here on the first row that is not NULL — spec §4)
// go: builtinCastDurationAsRealSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinCastDurationAsRealSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Real)]
pub fn cast_duration_as_real(
    _ctx: &dyn EvalContext,
    _f: &impl SigView,
    val: Duration,
) -> Result<Option<f64>, Error> {
    let val = checked_fsp(val)?;
    let (res, err) = val.to_number().to_float64();
    err?;
    Ok(Some(res))
}

// go: builtinCastDurationAsDecimalSig.evalDecimal L1
// go: builtinCastDurationAsDecimalSig.vecEvalDecimal L2 (the vector driver of this body; fsp check as for builtinCastDurationAsRealSig, spec §4)
// go: builtinCastDurationAsDecimalSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinCastDurationAsDecimalSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Decimal)]
pub fn cast_duration_as_decimal(
    ctx: &dyn EvalContext,
    f: &impl SigView,
    val: Duration,
) -> Result<Option<MyDecimal>, Error> {
    let val = checked_fsp(val)?;
    produce_dec(ctx, f, val.to_number())
}

// go: builtinCastDurationAsStringSig.evalString L1
// go: builtinCastDurationAsStringSig.vecEvalString L2 (the vector driver of this body; durations read with the argument's declared fsp, M-EVAL)
// go: builtinCastDurationAsStringSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinCastDurationAsStringSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(String)]
pub fn cast_duration_as_string(
    ctx: &dyn EvalContext,
    f: &impl SigView,
    val: Duration,
    out: &mut Vec<u8>,
) -> Result<Option<()>, Error> {
    produce_str(ctx, f, val.string(), out)
}

// go: builtinCastJSONAsIntSig.evalInt L1
// go: builtinCastJSONAsIntSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
// go: builtinCastJSONAsIntSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinCastJSONAsIntSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Int)]
pub fn cast_json_as_int(
    ctx: &dyn EvalContext,
    f: &impl SigView,
    val: &BinaryJSON<'_>,
) -> Result<Option<i64>, Error> {
    let (res, err) = convert_json_to_int64(&type_ctx(ctx), val, has_unsigned_flag(f.tp.get_flag()));
    if let Err(e) = err {
        err_ctx(ctx).handle_error(e)?;
    }
    Ok(Some(res))
}

// go: builtinCastJSONAsRealSig.evalReal L1
// go: builtinCastJSONAsRealSig.vecEvalReal L2 (the vector driver of this body, M-EVAL)
// go: builtinCastJSONAsRealSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinCastJSONAsRealSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Real)]
pub fn cast_json_as_real(
    ctx: &dyn EvalContext,
    _f: &impl SigView,
    val: &BinaryJSON<'_>,
) -> Result<Option<f64>, Error> {
    let (res, err) = convert_json_to_float(&type_ctx(ctx), val);
    err?;
    Ok(Some(res))
}

// go: builtinCastJSONAsDecimalSig.evalDecimal L1
// go: builtinCastJSONAsDecimalSig.vecEvalDecimal L2 (the vector driver of this body, M-EVAL)
// go: builtinCastJSONAsDecimalSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinCastJSONAsDecimalSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Decimal)]
pub fn cast_json_as_decimal(
    ctx: &dyn EvalContext,
    f: &impl SigView,
    val: &BinaryJSON<'_>,
) -> Result<Option<MyDecimal>, Error> {
    let (res, err) = convert_json_to_decimal(&type_ctx(ctx), val);
    err?;
    produce_dec(ctx, f, res)
}

// go: builtinCastJSONAsStringSig.evalString L1
// go: builtinCastJSONAsStringSig.vecEvalString L2 (the vector driver of this body, M-EVAL)
// go: builtinCastJSONAsStringSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinCastJSONAsStringSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(String)]
pub fn cast_json_as_string(
    ctx: &dyn EvalContext,
    f: &impl SigView,
    val: &BinaryJSON<'_>,
    out: &mut Vec<u8>,
) -> Result<Option<()>, Error> {
    let (s, err) = produce_str_with_specified_tp(val.string(), &f.tp, &type_ctx(ctx), false);
    err?;
    out.extend_from_slice(&s);
    Ok(Some(()))
}

// go: builtinCastVectorFloat32AsStringSig.evalString L1
// go: builtinCastVectorFloat32AsStringSig.Clone L2 (BuiltinFunc::clone_from_base)
// Go defines no vecEvalString for it (and vectorized() is false): `novec`.
#[eval_fn(String, novec)]
pub fn cast_vector_float32_as_string(
    ctx: &dyn EvalContext,
    f: &impl SigView,
    val: &VectorFloat32<'_>,
    out: &mut Vec<u8>,
) -> Result<Option<()>, Error> {
    let (s, err) =
        produce_str_with_specified_tp(val.string().into_bytes(), &f.tp, &type_ctx(ctx), false);
    err?;
    out.extend_from_slice(&s);
    Ok(Some(()))
}

// ---------------------------------------------------------------- to TIME (DATE, DATETIME, TIMESTAMP), DURATION

/// `types.ZeroTime, true, handleInvalidTimeError(ctx, err)`: NULL when the error is handled.
// glue: the shared `return types.ZeroTime, true, handleInvalidTimeError(ctx, err)` of the time signatures
fn invalid_time<T>(ctx: &dyn EvalContext, err: Error) -> Result<Option<T>, Error> {
    handle_invalid_time_error(ctx, err)?;
    Ok(None)
}

/// Truncates the hh:mm:ss part of `t` when the result type is DATE (Go's
/// `res.SetCoreTime(types.FromDate(res.Year(), res.Month(), res.Day(), 0, 0, 0, 0))`).
// glue: the shared DATE truncation of the CAST AS TIME signatures
fn truncate_to_date(t: &mut Time, tp: u8) {
    if tp == TypeDate {
        t.set_core_time(from_date(t.year(), t.month(), t.day(), 0, 0, 0, 0));
    }
}

/// `getStmtTimestamp(ctx)` (`ctx.CurrentTime()`; the `intest` random location and the
/// `injectNow` failpoint are test-only), falling back to `gotime.Now()` on error as the CAST
/// signatures do.
// glue: boundary stub for getStmtTimestamp (helper.go, block D08.12) and the gotime.Now() fallback of the CAST signatures
fn stmt_timestamp(ctx: &dyn EvalContext) -> gotime::Time {
    ctx.current_time().unwrap_or_else(|_| gotime::now())
}

/// `errors.Is(err, strconv.ErrSyntax) || errors.Is(err, strconv.ErrRange)`: the
/// `*strconv.NumError` of `types.ParseTime` (an error without a code whose text is
/// `strconv.<Func>: parsing <q>: invalid syntax | value out of range`).
// glue: errors.Is on the *strconv.NumError text (M-ERR)
fn is_strconv_num_error(err: &Error) -> bool {
    if err.is_normalized() {
        return false;
    }
    let msg = err.error();
    msg.starts_with(b"strconv.")
        && (msg.ends_with(b": invalid syntax") || msg.ends_with(b": value out of range"))
}

// go: builtinCastIntAsTimeSig.evalTime L1
// go: builtinCastIntAsTimeSig.vecEvalTime L2 (the vector driver of this body, M-EVAL)
// go: builtinCastIntAsTimeSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinCastIntAsTimeSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Time)]
pub fn cast_int_as_time(
    ctx: &dyn EvalContext,
    f: &impl SigView,
    val: i64,
) -> Result<Option<Time>, Error> {
    let r = if f.arg_type(0).get_type() == TypeYear {
        parse_time_from_year(val)
    } else {
        let (t, err) =
            parse_time_from_num(&type_ctx(ctx), val, f.tp.get_type(), f.tp.get_decimal());
        err.map(|()| t)
    };
    let mut res = match r {
        Ok(t) => t,
        Err(e) => return invalid_time(ctx, e),
    };
    truncate_to_date(&mut res, f.tp.get_type());
    Ok(Some(res))
}

// go: builtinCastRealAsTimeSig.evalTime L1
// go: builtinCastRealAsTimeSig.vecEvalTime L2 (the vector driver of this body, M-EVAL)
// go: builtinCastRealAsTimeSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinCastRealAsTimeSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Time)]
pub fn cast_real_as_time(
    ctx: &dyn EvalContext,
    f: &impl SigView,
    val: f64,
) -> Result<Option<Time>, Error> {
    // MySQL compatibility: 0 should not be converted to null, see #11203
    let fv = format_float(val, b'f', -1, 64);
    if fv == "0" {
        return Ok(Some(ZeroTime));
    }
    let (mut res, err) = parse_time_from_float_string(
        &type_ctx(ctx),
        fv.as_bytes(),
        f.tp.get_type(),
        f.tp.get_decimal(),
    );
    if let Err(e) = err {
        return invalid_time(ctx, e);
    }
    truncate_to_date(&mut res, f.tp.get_type());
    Ok(Some(res))
}

// go: builtinCastDecimalAsTimeSig.evalTime L1
// go: builtinCastDecimalAsTimeSig.vecEvalTime L2 (the vector driver of this body, M-EVAL)
// go: builtinCastDecimalAsTimeSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinCastDecimalAsTimeSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Time)]
pub fn cast_decimal_as_time(
    ctx: &dyn EvalContext,
    f: &impl SigView,
    val: &MyDecimal,
) -> Result<Option<Time>, Error> {
    let (mut res, err) = parse_time_from_float_string(
        &type_ctx(ctx),
        &val.to_string(),
        f.tp.get_type(),
        f.tp.get_decimal(),
    );
    if let Err(e) = err {
        return invalid_time(ctx, e);
    }
    truncate_to_date(&mut res, f.tp.get_type());
    Ok(Some(res))
}

// go: builtinCastStringAsTimeSig.evalTime L1
// go: builtinCastStringAsTimeSig.vecEvalTime L2 (the vector driver of this body; only it turns a *strconv.NumError of ParseTime into ErrIncorrectDatetimeValue, as in Go)
// go: builtinCastStringAsTimeSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinCastStringAsTimeSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Time)]
pub fn cast_string_as_time(
    ctx: &dyn EvalContext,
    f: &impl SigView,
    val: &[u8],
) -> Result<Option<Time>, Error> {
    let (mut res, err) = parse_time(&type_ctx(ctx), val, f.tp.get_type(), f.tp.get_decimal());
    if let Err(mut e) = err {
        if f.vector() && is_strconv_num_error(&e) {
            e = ErrIncorrectDatetimeValue
                .gen_with_stack_by_args(vec![Arg::go_string(val.to_vec())]);
        }
        return invalid_time(ctx, e);
    }
    if res.is_zero() && sql_mode(ctx).has_no_zero_date_mode() {
        return invalid_time(
            ctx,
            ErrWrongValue
                .gen_with_stack_by_args(vec![Arg::from(DateTimeStr), Arg::go_string(res.string())]),
        );
    }
    truncate_to_date(&mut res, f.tp.get_type());
    Ok(Some(res))
}

// go: builtinCastTimeAsTimeSig.evalTime L1
// go: builtinCastTimeAsTimeSig.vecEvalTime L2 (the vector driver of this body, M-EVAL)
// go: builtinCastTimeAsTimeSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinCastTimeAsTimeSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Time)]
pub fn cast_time_as_time(
    ctx: &dyn EvalContext,
    f: &impl SigView,
    val: Time,
) -> Result<Option<Time>, Error> {
    let tc = type_ctx(ctx);
    let (res, err) = val.convert(&tc, f.tp.get_type());
    if let Err(e) = err {
        return invalid_time(ctx, e);
    }
    let (mut res, err) = res.round_frac(&tc, f.tp.get_decimal());
    if f.tp.get_type() == TypeDate {
        truncate_to_date(&mut res, TypeDate);
        res.set_type(f.tp.get_type());
    }
    err?;
    Ok(Some(res))
}

// go: builtinCastDurationAsTimeSig.evalTime L1
// go: builtinCastDurationAsTimeSig.vecEvalTime L2 (the vector driver of this body; it reads the durations with the result's fsp, the row version with the value's, as in Go)
// go: builtinCastDurationAsTimeSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinCastDurationAsTimeSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Time)]
pub fn cast_duration_as_time(
    ctx: &dyn EvalContext,
    f: &impl SigView,
    mut val: Duration,
) -> Result<Option<Time>, Error> {
    if f.vector() {
        val.fsp = f.tp.get_decimal();
    }
    let tc = type_ctx(ctx);
    let ts = stmt_timestamp(ctx);
    let (res, err) = val.convert_to_time_with_timestamp(&tc, f.tp.get_type(), &ts);
    if let Err(e) = err {
        return invalid_time(ctx, e);
    }
    let (res, err) = res.round_frac(&tc, f.tp.get_decimal());
    err?;
    Ok(Some(res))
}

// go: builtinCastJSONAsTimeSig.evalTime L1
// go: builtinCastJSONAsTimeSig.vecEvalTime L2 (the vector driver of this body; its default branch goes through handleInvalidTimeError, which handles ErrTruncatedWrongVal as ec.HandleError does)
// go: builtinCastJSONAsTimeSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinCastJSONAsTimeSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Time)]
pub fn cast_json_as_time(
    ctx: &dyn EvalContext,
    f: &impl SigView,
    val: &BinaryJSON<'_>,
) -> Result<Option<Time>, Error> {
    let (tp, fsp) = (f.tp.get_type(), f.tp.get_decimal());
    match val.type_code {
        JSONTypeCodeDate | JSONTypeCodeDatetime | JSONTypeCodeTimestamp => {
            let mut res = val.get_time_with_fsp(fsp);
            res.set_type(tp);
            truncate_to_date(&mut res, tp);
            Ok(Some(res))
        }
        JSONTypeCodeDuration => {
            let duration = val.get_duration();
            let tc = type_ctx(ctx);
            let ts = stmt_timestamp(ctx);
            let (res, err) = duration.convert_to_time_with_timestamp(&tc, tp, &ts);
            if let Err(e) = err {
                return invalid_time(ctx, e);
            }
            let (res, err) = res.round_frac(&tc, fsp);
            err?;
            Ok(Some(res))
        }
        JSONTypeCodeString => {
            let s = val.unquote()?;
            let (mut res, err) = parse_time(&type_ctx(ctx), &s, tp, fsp);
            if let Err(e) = err {
                return invalid_time(ctx, e);
            }
            truncate_to_date(&mut res, tp);
            Ok(Some(res))
        }
        _ => {
            let err = ErrTruncatedWrongVal.gen_with_stack_by_args(vec![
                Arg::from(type_str(tp)),
                Arg::go_string(val.string()),
            ]);
            if f.vector() {
                return invalid_time(ctx, err);
            }
            err_ctx(ctx).handle_error(err)?;
            Ok(None)
        }
    }
}

// go: builtinCastIntAsDurationSig.evalDuration L1
// go: builtinCastIntAsDurationSig.vecEvalDuration L2 (the vector driver of this body, M-EVAL)
// go: builtinCastIntAsDurationSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinCastIntAsDurationSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Duration)]
pub fn cast_int_as_duration(
    ctx: &dyn EvalContext,
    f: &impl SigView,
    val: i64,
) -> Result<Option<Duration>, Error> {
    let (dur, err) = number_to_duration(val, f.tp.get_decimal());
    match err {
        Ok(()) => Ok(Some(dur)),
        Err(e) => {
            if ErrOverflow.equal(&e) || ErrTruncatedWrongVal.equal(&e) {
                err_ctx(ctx).handle_error(e)?;
                return Ok(None);
            }
            Err(e)
        }
    }
}

// go: builtinCastRealAsDurationSig.evalDuration L1
// go: builtinCastRealAsDurationSig.vecEvalDuration L2 (the vector driver of this body, M-EVAL)
// go: builtinCastRealAsDurationSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinCastRealAsDurationSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Duration)]
pub fn cast_real_as_duration(
    ctx: &dyn EvalContext,
    f: &impl SigView,
    val: f64,
) -> Result<Option<Duration>, Error> {
    let tc = type_ctx(ctx);
    let (res, _, err) = parse_duration(
        &tc,
        format_float(val, b'f', -1, 64).as_bytes(),
        f.tp.get_decimal(),
    );
    match err {
        Ok(()) => Ok(Some(res)),
        Err(e) if ErrTruncatedWrongVal.equal(&e) => {
            tc.handle_truncate(Err(e))?;
            // ErrTruncatedWrongVal needs to be considered NULL.
            Ok(None)
        }
        Err(e) => Err(e),
    }
}

// go: builtinCastDecimalAsDurationSig.evalDuration L1
// go: builtinCastDecimalAsDurationSig.vecEvalDuration L2 (the vector driver of this body; it handles ErrTruncatedWrongVal with tc.HandleTruncate, the row version with ec.HandleError, as in Go)
// go: builtinCastDecimalAsDurationSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinCastDecimalAsDurationSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Duration)]
pub fn cast_decimal_as_duration(
    ctx: &dyn EvalContext,
    f: &impl SigView,
    val: &MyDecimal,
) -> Result<Option<Duration>, Error> {
    let tc = type_ctx(ctx);
    let (res, _, err) = parse_duration(&tc, &val.to_string(), f.tp.get_decimal());
    match err {
        Ok(()) => Ok(Some(res)),
        Err(e) if ErrTruncatedWrongVal.equal(&e) => {
            if f.vector() {
                tc.handle_truncate(Err(e))?;
            } else {
                err_ctx(ctx).handle_error(e)?;
            }
            // ErrTruncatedWrongVal needs to be considered NULL.
            Ok(None)
        }
        Err(e) => Err(e),
    }
}

// go: builtinCastStringAsDurationSig.evalDuration L1
// go: builtinCastStringAsDurationSig.vecEvalDuration L2 (the vector driver of this body; it handles ErrTruncatedWrongVal with tc.HandleTruncate and looks at isNull only after an error, the row version uses ec.HandleError and returns isNull as it is, as in Go)
// go: builtinCastStringAsDurationSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinCastStringAsDurationSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Duration)]
pub fn cast_string_as_duration(
    ctx: &dyn EvalContext,
    f: &impl SigView,
    val: &[u8],
) -> Result<Option<Duration>, Error> {
    let tc = type_ctx(ctx);
    let (res, is_null, err) = parse_duration(&tc, val, f.tp.get_decimal());
    if f.vector() {
        if let Err(e) = err {
            if ErrTruncatedWrongVal.equal(&e) {
                tc.handle_truncate(Err(e))?;
            } else {
                return Err(e);
            }
            if is_null {
                return Ok(None);
            }
        }
        return Ok(Some(res));
    }
    match err {
        Err(e) if ErrTruncatedWrongVal.equal(&e) => err_ctx(ctx).handle_error(e)?,
        other => other?,
    }
    Ok(if is_null { None } else { Some(res) })
}

// go: builtinCastTimeAsDurationSig.evalDuration L1
// go: builtinCastTimeAsDurationSig.vecEvalDuration L2 (the vector driver of this body, M-EVAL)
// go: builtinCastTimeAsDurationSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinCastTimeAsDurationSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Duration)]
pub fn cast_time_as_duration(
    ctx: &dyn EvalContext,
    f: &impl SigView,
    val: Time,
) -> Result<Option<Duration>, Error> {
    let res = val.convert_to_duration()?;
    let (res, err) = res.round_frac(f.tp.get_decimal(), &location(ctx));
    err?;
    Ok(Some(res))
}

// go: builtinCastDurationAsDurationSig.evalDuration L1
// go: builtinCastDurationAsDurationSig.vecEvalDuration L2 (the vector driver of this body; it rounds from an unspecified fsp, the row version from the value's — RoundFrac returns the value unchanged when the fsp is already the target's — as in Go)
// go: builtinCastDurationAsDurationSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinCastDurationAsDurationSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Duration)]
pub fn cast_duration_as_duration(
    ctx: &dyn EvalContext,
    f: &impl SigView,
    mut val: Duration,
) -> Result<Option<Duration>, Error> {
    if f.vector() {
        val.fsp = UnspecifiedFsp;
    }
    let (res, err) = val.round_frac(f.tp.get_decimal(), &location(ctx));
    err?;
    Ok(Some(res))
}

// go: builtinCastJSONAsDurationSig.evalDuration L1
// go: builtinCastJSONAsDurationSig.vecEvalDuration L2 (the vector driver of this body; its default branch's warning names the type with types.TypeStr ("time"), the row version's with "TIME", as in Go)
// go: builtinCastJSONAsDurationSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinCastJSONAsDurationSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Duration)]
pub fn cast_json_as_duration(
    ctx: &dyn EvalContext,
    f: &impl SigView,
    val: &BinaryJSON<'_>,
) -> Result<Option<Duration>, Error> {
    let tc = type_ctx(ctx);
    let fsp = f.tp.get_decimal();
    match val.type_code {
        JSONTypeCodeDate | JSONTypeCodeDatetime | JSONTypeCodeTimestamp => {
            let time = val.get_time_with_fsp(fsp);
            let res = time.convert_to_duration()?;
            let (res, err) = res.round_frac(fsp, &location(ctx));
            err?;
            Ok(Some(res))
        }
        JSONTypeCodeDuration => Ok(Some(val.get_duration())),
        JSONTypeCodeString => {
            let s = val.unquote()?;
            let (res, _, err) = parse_duration(&tc, &s, fsp);
            match err {
                Err(e) if ErrTruncatedWrongVal.equal(&e) => tc.handle_truncate(Err(e))?,
                other => other?,
            }
            Ok(Some(res))
        }
        _ => {
            let name = if f.vector() {
                type_str(f.tp.get_type())
            } else {
                "TIME"
            };
            let err = ErrTruncatedWrongVal
                .gen_with_stack_by_args(vec![Arg::from(name), Arg::go_string(val.string())]);
            tc.handle_truncate(Err(err))?;
            Ok(None)
        }
    }
}

// go: builtinCastIntAsJSONSig.evalJSON L1
// go: builtinCastIntAsJSONSig.vecEvalJSON L2 (the vector driver of this body, M-EVAL)
// go: builtinCastIntAsJSONSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinCastIntAsJSONSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Json)]
pub fn cast_int_as_json(
    _ctx: &dyn EvalContext,
    f: &impl SigView,
    val: i64,
) -> Result<Option<BinaryJSON<'static>>, Error> {
    let arg_tp = f.arg_type(0);
    let res = if has_is_boolean_flag(arg_tp.get_flag()) {
        create_binary_json(JSONValue::Bool(val != 0))
    } else if has_unsigned_flag(arg_tp.get_flag()) || arg_tp.get_type() == TypeYear {
        create_binary_json(JSONValue::Uint64(val as u64))
    } else {
        create_binary_json(JSONValue::Int64(val))
    };
    Ok(Some(res))
}

// go: builtinCastRealAsJSONSig.evalJSON L1
// go: builtinCastRealAsJSONSig.vecEvalJSON L2 (the vector driver of this body, M-EVAL)
// go: builtinCastRealAsJSONSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinCastRealAsJSONSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Json)]
pub fn cast_real_as_json(
    _ctx: &dyn EvalContext,
    _f: &impl SigView,
    val: f64,
) -> Result<Option<BinaryJSON<'static>>, Error> {
    // FIXME: `select json_type(cast(1111.11 as json))` should return `DECIMAL`, we return `DOUBLE` now.
    Ok(Some(create_binary_json(JSONValue::Float64(val))))
}

// go: builtinCastDecimalAsJSONSig.evalJSON L1
// go: builtinCastDecimalAsJSONSig.vecEvalJSON L2 (the vector driver of this body, M-EVAL)
// go: builtinCastDecimalAsJSONSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinCastDecimalAsJSONSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Json)]
pub fn cast_decimal_as_json(
    _ctx: &dyn EvalContext,
    _f: &impl SigView,
    val: &MyDecimal,
) -> Result<Option<BinaryJSON<'static>>, Error> {
    // FIXME: `select json_type(cast(1111.11 as json))` should return `DECIMAL`, we return `DOUBLE` now.
    let (f64v, err) = val.to_float64();
    err?;
    Ok(Some(create_binary_json(JSONValue::Float64(f64v))))
}

// go: builtinCastStringAsJSONSig.evalJSON L1
// go: builtinCastStringAsJSONSig.vecEvalJSON L2 (the vector driver of this body, M-EVAL)
// go: builtinCastStringAsJSONSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinCastStringAsJSONSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Json)]
pub fn cast_string_as_json(
    _ctx: &dyn EvalContext,
    f: &impl SigView,
    val: &[u8],
) -> Result<Option<BinaryJSON<'static>>, Error> {
    let typ = f.arg_type(0);
    if is_binary_str(&typ) {
        let mut buf = val.to_vec();
        if typ.get_type() == TypeString && typ.get_flen() > 0 {
            // the tailing zero should also be in the opaque json
            let flen = typ.get_flen() as usize;
            buf = vec![0; flen];
            let m = flen.min(val.len());
            buf[..m].copy_from_slice(&val[..m]);
        }
        return Ok(Some(create_binary_json(JSONValue::Opaque(Opaque {
            type_code: typ.get_type(),
            buf: buf.into(),
        }))));
    }
    if has_parse_to_json_flag(f.tp.get_flag()) {
        return parse_binary_json_from_string(val).map(Some);
    }
    Ok(Some(create_binary_json(JSONValue::String(val.to_vec()))))
}

// go: builtinCastDurationAsJSONSig.evalJSON L1
// go: builtinCastDurationAsJSONSig.vecEvalJSON L2 (the vector driver of this body, M-EVAL)
// go: builtinCastDurationAsJSONSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinCastDurationAsJSONSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Json)]
pub fn cast_duration_as_json(
    _ctx: &dyn EvalContext,
    _f: &impl SigView,
    mut val: Duration,
) -> Result<Option<BinaryJSON<'static>>, Error> {
    val.fsp = MaxFsp;
    Ok(Some(create_binary_json(JSONValue::Duration(val))))
}

// go: builtinCastTimeAsJSONSig.evalJSON L1
// go: builtinCastTimeAsJSONSig.vecEvalJSON L2 (the vector driver of this body, M-EVAL)
// go: builtinCastTimeAsJSONSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinCastTimeAsJSONSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Json)]
pub fn cast_time_as_json(
    _ctx: &dyn EvalContext,
    _f: &impl SigView,
    mut val: Time,
) -> Result<Option<BinaryJSON<'static>>, Error> {
    if val.tp() == TypeDatetime || val.tp() == TypeTimestamp {
        val.set_fsp(MaxFsp);
    }
    Ok(Some(create_binary_json(JSONValue::Time(val))))
}

// go: builtinCastJSONAsJSONSig.evalJSON L1
// go: builtinCastJSONAsJSONSig.vecEvalJSON L2 (the vector driver of this body, M-EVAL)
// go: builtinCastJSONAsJSONSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinCastJSONAsJSONSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Json)]
pub fn cast_json_as_json(
    _ctx: &dyn EvalContext,
    _f: &impl SigView,
    val: &BinaryJSON<'_>,
) -> Result<Option<BinaryJSON<'static>>, Error> {
    Ok(Some(val.clone().into_owned()))
}

/// `fakeSctx`: `cast as array` and `ConvertJSON2Tp` ignore the SQL mode and return every
/// error (`stmtctx.NewStmtCtx()` with `types.StrictFlags`, UTC, its own warnings).
// go: newFakeSctx L2 (the package variable fakeSctx is its TypeCtx, types.StrictContext; its error context returns every error)
fn fake_sctx() -> &'static TypeContext {
    &StrictContext
}

// go: castJSONAsArrayFunctionSig.evalJSON L1
// go: castJSONAsArrayFunctionSig.Clone L2 (BuiltinFunc::clone_from_base)
#[eval_fn(Json, novec)]
pub fn cast_json_as_array(
    _ctx: &dyn EvalContext,
    f: &impl SigView,
    val: &BinaryJSON<'_>,
) -> Result<Option<BinaryJSON<'static>>, Error> {
    if val.type_code == JSONTypeCodeObject {
        return Err(ErrNotSupportedYet
            .gen_with_stack_by_args(vec![Arg::from("CAST-ing JSON OBJECT type to array")]));
    }
    let ft = f.tp.array_type();
    let Some(conv) = convert_json2_tp_fn(ft.eval_type()) else {
        return Err(
            ErrNotSupportedYet.gen_with_stack_by_args(vec![Arg::from(format!(
                "CAS-ing data to array of {}",
                ft.to_string()
            ))]),
        );
    };
    let mut array_vals = Vec::with_capacity(8);
    if val.type_code != JSONTypeCodeArray {
        array_vals.push(conv(fake_sctx(), val, &ft)?);
    } else {
        for i in 0..val.get_elem_count() {
            array_vals.push(conv(fake_sctx(), &val.array_get_elem(i), &ft)?);
        }
    }
    Ok(Some(create_binary_json(JSONValue::Array(array_vals))))
}

/// The converter `convertJSON2Tp` returns for an evaluation type.
type ConvertJSON2TpFn =
    fn(&TypeContext, &BinaryJSON<'_>, &FieldType) -> Result<JSONValue<'static>, Error>;

/// ConvertJSON2Tp converts JSON to the specified type.
// go: ConvertJSON2Tp L1
pub fn convert_json2_tp(
    v: &BinaryJSON<'_>,
    target_type: &FieldType,
) -> Result<JSONValue<'static>, Error> {
    let Some(convert_func) = convert_json2_tp_fn(target_type.eval_type()) else {
        return Err(ErrInvalidJSONForFuncIndex.clone());
    };
    convert_func(fake_sctx(), v, target_type)
}

// go: convertJSON2Tp L2 (the function values are fn items; the StatementContext is its TypeCtx, whose error context returns every error)
fn convert_json2_tp_fn(eval_type: EvalType) -> Option<ConvertJSON2TpFn> {
    let f: ConvertJSON2TpFn = match eval_type {
        ETString => |tc, item, tp| {
            if item.type_code != JSONTypeCodeString {
                return Err(ErrInvalidJSONForFuncIndex.clone());
            }
            let (s, err) = produce_str_with_specified_tp(item.get_string().to_vec(), tp, tc, false);
            err?;
            Ok(JSONValue::String(s))
        },
        ETInt => |tc, item, tp| {
            if item.type_code != JSONTypeCodeInt64 && item.type_code != JSONTypeCodeUint64 {
                return Err(ErrInvalidJSONForFuncIndex.clone());
            }
            let unsigned = has_unsigned_flag(tp.get_flag());
            let (json_to_int, err) = convert_json_to_int(tc, item, unsigned, tp.get_type());
            err?;
            if unsigned {
                return Ok(JSONValue::Uint64(json_to_int as u64));
            }
            Ok(JSONValue::Int64(json_to_int))
        },
        ETReal => |tc, item, _tp| {
            if item.type_code != JSONTypeCodeFloat64
                && item.type_code != JSONTypeCodeInt64
                && item.type_code != JSONTypeCodeUint64
            {
                return Err(ErrInvalidJSONForFuncIndex.clone());
            }
            let (f, err) = convert_json_to_float(tc, item);
            err?;
            Ok(JSONValue::Float64(f))
        },
        ETDatetime => |_tc, item, tp| {
            if (tp.get_type() == TypeDatetime && item.type_code != JSONTypeCodeDatetime)
                || (tp.get_type() == TypeDate && item.type_code != JSONTypeCodeDate)
            {
                return Err(ErrInvalidJSONForFuncIndex.clone());
            }
            let mut res = item.get_time_with_fsp(tp.get_decimal());
            res.set_type(tp.get_type());
            if tp.get_type() == TypeDate {
                // Truncate hh:mm:ss part if the type is Date.
                res.set_core_time(from_date(res.year(), res.month(), res.day(), 0, 0, 0, 0));
            }
            Ok(JSONValue::Time(res))
        },
        ETDuration => |_tc, item, _tp| {
            if item.type_code != JSONTypeCodeDuration {
                return Err(ErrInvalidJSONForFuncIndex.clone());
            }
            Ok(JSONValue::Duration(item.get_duration()))
        },
        _ => return None,
    };
    Some(f)
}

// go: builtinCastUnsupportedAsVectorFloat32Sig.evalVectorFloat32 L1
// go: builtinCastUnsupportedAsVectorFloat32Sig.Clone L2 (BuiltinFunc::clone_from_base)
#[eval_fn(VectorFloat32, lazy, novec)]
pub fn cast_unsupported_as_vector_float32(
    _ctx: &dyn EvalContext,
    f: &impl SigView,
    _args: &mut impl Args,
) -> Result<Option<VectorFloat32<'static>>, Error> {
    Err(Error::plain(format!(
        "cannot cast from {} to vector",
        type_str(f.arg_type(0).get_type())
    )))
}

// go: builtinCastStringAsVectorFloat32Sig.evalVectorFloat32 L1
// go: builtinCastStringAsVectorFloat32Sig.Clone L2 (BuiltinFunc::clone_from_base)
#[eval_fn(VectorFloat32, novec)]
pub fn cast_string_as_vector_float32(
    _ctx: &dyn EvalContext,
    f: &impl SigView,
    val: &[u8],
) -> Result<Option<VectorFloat32<'static>>, Error> {
    let vec = parse_vector_float32(val)?;
    vec.check_dims_fit_column(f.tp.get_flen())?;
    Ok(Some(vec))
}

// go: builtinCastVectorFloat32AsVectorFloat32Sig.evalVectorFloat32 L1
// go: builtinCastVectorFloat32AsVectorFloat32Sig.Clone L2 (BuiltinFunc::clone_from_base)
#[eval_fn(VectorFloat32, novec)]
pub fn cast_vector_float32_as_vector_float32(
    _ctx: &dyn EvalContext,
    f: &impl SigView,
    val: &VectorFloat32<'_>,
) -> Result<Option<VectorFloat32<'static>>, Error> {
    val.check_dims_fit_column(f.tp.get_flen())?;
    Ok(Some(val.clone()))
}

// go: builtinCastVectorFloat32AsUnsupportedSig.evalInt L1
// go: builtinCastVectorFloat32AsUnsupportedSig.evalReal L1
// go: builtinCastVectorFloat32AsUnsupportedSig.evalDecimal L1
// go: builtinCastVectorFloat32AsUnsupportedSig.evalString L1
// go: builtinCastVectorFloat32AsUnsupportedSig.evalTime L1
// go: builtinCastVectorFloat32AsUnsupportedSig.evalDuration L1
// go: builtinCastVectorFloat32AsUnsupportedSig.evalJSON L1
// go: builtinCastVectorFloat32AsUnsupportedSig.Clone L2 (BuiltinFunc::clone_from_base)
fn cast_vector_float32_as_unsupported(f: &BuiltinFunc, _ctx: &dyn EvalContext) -> Error {
    Error::plain(format!(
        "cannot cast from vector to {}",
        type_str(f.tp.get_type())
    ))
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

/// `castAsTimeFunctionClass`.
#[derive(Clone, Debug)]
pub struct CastAsTimeFunctionClass {
    /// The embedded `baseFunctionClass`.
    pub base: BaseFunctionClass,
    /// `tp`.
    pub tp: FieldType,
}

/// `castAsDurationFunctionClass`.
#[derive(Clone, Debug)]
pub struct CastAsDurationFunctionClass {
    /// The embedded `baseFunctionClass`.
    pub base: BaseFunctionClass,
    /// `tp`.
    pub tp: FieldType,
}

impl CastAsTimeFunctionClass {
    // go: castAsTimeFunctionClass.getFunction L1
    pub fn get_function(
        &self,
        ctx: &dyn BuildContext,
        args: Vec<ExprRef>,
    ) -> Result<BuiltinFunc, Error> {
        self.base.verify_args(&args)?;
        let bf = new_base_builtin_func(ctx, self.base.func_name, args, self.tp.clone())?;
        let arg_tp = bf.args[0].get_type(&*ctx.get_eval_ctx()).eval_type();
        Ok(match arg_tp {
            ETInt => bf.with_sig(&builtinCastIntAsTimeSig),
            ETReal => bf.with_sig(&builtinCastRealAsTimeSig),
            ETDecimal => bf.with_sig(&builtinCastDecimalAsTimeSig),
            ETDatetime | ETTimestamp => bf.with_sig(&builtinCastTimeAsTimeSig),
            ETDuration => bf.with_sig(&builtinCastDurationAsTimeSig),
            ETJson => bf.with_sig(&builtinCastJSONAsTimeSig),
            ETString => bf.with_sig(&builtinCastStringAsTimeSig),
            ETVectorFloat32 => bf.with_sig(&builtinCastVectorFloat32AsUnsupportedSig),
            _ => return Err(cannot_cast(arg_tp, "Datetime")),
        })
    }
}

impl CastAsDurationFunctionClass {
    // go: castAsDurationFunctionClass.getFunction L1
    pub fn get_function(
        &self,
        ctx: &dyn BuildContext,
        args: Vec<ExprRef>,
    ) -> Result<BuiltinFunc, Error> {
        self.base.verify_args(&args)?;
        let bf = new_base_builtin_func(ctx, self.base.func_name, args, self.tp.clone())?;
        let arg_tp = bf.args[0].get_type(&*ctx.get_eval_ctx()).eval_type();
        Ok(match arg_tp {
            ETInt => bf.with_sig(&builtinCastIntAsDurationSig),
            ETReal => bf.with_sig(&builtinCastRealAsDurationSig),
            ETDecimal => bf.with_sig(&builtinCastDecimalAsDurationSig),
            ETDatetime | ETTimestamp => bf.with_sig(&builtinCastTimeAsDurationSig),
            ETDuration => bf.with_sig(&builtinCastDurationAsDurationSig),
            ETJson => bf.with_sig(&builtinCastJSONAsDurationSig),
            ETString => bf.with_sig(&builtinCastStringAsDurationSig),
            ETVectorFloat32 => bf.with_sig(&builtinCastVectorFloat32AsUnsupportedSig),
            _ => return Err(cannot_cast(arg_tp, "Time")),
        })
    }
}

/// `castAsArrayFunctionClass`.
#[derive(Clone, Debug)]
pub struct CastAsArrayFunctionClass {
    /// The embedded `baseFunctionClass`.
    pub base: BaseFunctionClass,
    /// `tp`.
    pub tp: FieldType,
}

impl CastAsArrayFunctionClass {
    // go: castAsArrayFunctionClass.verifyArgs L1
    fn verify_args(&self, ctx: &dyn EvalContext, args: &[ExprRef]) -> Result<(), Error> {
        self.base.verify_args(args)?;
        if args[0].get_type(ctx).eval_type() != ETJson {
            return Err(ErrInvalidTypeForJSON
                .gen_with_stack_by_args(vec![Arg::from(1isize), Arg::from("cast_as_array")]));
        }
        Ok(())
    }

    // go: castAsArrayFunctionClass.getFunction L1
    pub fn get_function(
        &self,
        ctx: &dyn BuildContext,
        args: Vec<ExprRef>,
    ) -> Result<BuiltinFunc, Error> {
        self.verify_args(&*ctx.get_eval_ctx(), &args)?;
        let array_type = self.tp.array_type();
        if matches!(
            array_type.get_type(),
            TypeYear | TypeJSON | TypeFloat | TypeNewDecimal
        ) {
            return Err(
                ErrNotSupportedYet.gen_with_stack_by_args(vec![Arg::from(format!(
                    "CAST-ing data to array of {}",
                    array_type.to_string()
                ))]),
            );
        }
        if array_type.eval_type() == ETString
            && array_type.get_charset() != CharsetUTF8MB4
            && array_type.get_charset() != CharsetBin
        {
            return Err(ErrNotSupportedYet.gen_with_stack_by_args(vec![Arg::from(
                "specifying charset for multi-valued index",
            )]));
        }
        if array_type.eval_type() == ETString && array_type.get_flen() == UnspecifiedLength {
            return Err(ErrNotSupportedYet.gen_with_stack_by_args(vec![Arg::from(
                "CAST-ing data to array of char/binary BLOBs with unspecified length",
            )]));
        }
        let bf = new_base_builtin_func(ctx, self.base.func_name, args, self.tp.clone())?;
        Ok(bf.with_sig(&castJSONAsArrayFunctionSig))
    }
}

/// `castAsJSONFunctionClass`.
#[derive(Clone, Debug)]
pub struct CastAsJSONFunctionClass {
    /// The embedded `baseFunctionClass`.
    pub base: BaseFunctionClass,
    /// `tp`.
    pub tp: FieldType,
}

impl CastAsJSONFunctionClass {
    // go: castAsJSONFunctionClass.getFunction L1
    pub fn get_function(
        &self,
        ctx: &dyn BuildContext,
        args: Vec<ExprRef>,
    ) -> Result<BuiltinFunc, Error> {
        self.base.verify_args(&args)?;
        let bf = new_base_builtin_func(ctx, self.base.func_name, args, self.tp.clone())?;
        let arg_tp = bf.args[0].get_type(&*ctx.get_eval_ctx()).eval_type();
        Ok(match arg_tp {
            ETInt => bf.with_sig(&builtinCastIntAsJSONSig),
            ETReal => bf.with_sig(&builtinCastRealAsJSONSig),
            ETDecimal => bf.with_sig(&builtinCastDecimalAsJSONSig),
            ETDatetime | ETTimestamp => bf.with_sig(&builtinCastTimeAsJSONSig),
            ETDuration => bf.with_sig(&builtinCastDurationAsJSONSig),
            ETJson => bf.with_sig(&builtinCastJSONAsJSONSig),
            ETString => {
                let mut sig = bf.with_sig(&builtinCastStringAsJSONSig);
                sig.tp.add_flag(ParseToJSONFlag);
                sig
            }
            ETVectorFloat32 => bf.with_sig(&builtinCastVectorFloat32AsUnsupportedSig),
            _ => return Err(cannot_cast(arg_tp, "Json")),
        })
    }
}

/// `castAsVectorFloat32FunctionClass`.
#[derive(Clone, Debug)]
pub struct CastAsVectorFloat32FunctionClass {
    /// The embedded `baseFunctionClass`.
    pub base: BaseFunctionClass,
    /// `tp`.
    pub tp: FieldType,
}

impl CastAsVectorFloat32FunctionClass {
    // go: castAsVectorFloat32FunctionClass.getFunction L1
    pub fn get_function(
        &self,
        ctx: &dyn BuildContext,
        args: Vec<ExprRef>,
    ) -> Result<BuiltinFunc, Error> {
        self.base.verify_args(&args)?;
        let bf = new_base_builtin_func(ctx, self.base.func_name, args, self.tp.clone())?;
        let arg_tp = bf.args[0].get_type(&*ctx.get_eval_ctx()).eval_type();
        Ok(match arg_tp {
            ETInt | ETReal | ETDecimal | ETDatetime | ETTimestamp | ETDuration | ETJson => {
                bf.with_sig(&builtinCastUnsupportedAsVectorFloat32Sig)
            }
            ETVectorFloat32 => bf.with_sig(&builtinCastVectorFloat32AsVectorFloat32Sig),
            ETString => bf.with_sig(&builtinCastStringAsVectorFloat32Sig),
            _ => return Err(cannot_cast(arg_tp, "VectorFloat32")),
        })
    }
}

/// `bf.baseBuiltinFunc`: the base without the cast part.
// glue: field access bf.baseBuiltinFunc of a baseBuiltinCastFunc
fn base_of(mut bf: BuiltinFunc) -> BuiltinFunc {
    bf.extra = crate::eval::SigExtra::None;
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
                base_of(bf).with_sig(&builtinCastVectorFloat32AsUnsupportedSig)
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
            ETVectorFloat32 => base_of(bf).with_sig(&builtinCastVectorFloat32AsUnsupportedSig),
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
            ETVectorFloat32 => base_of(bf).with_sig(&builtinCastVectorFloat32AsUnsupportedSig),
            _ => return Err(cannot_cast(arg_tp, "Decimal")),
        })
    }
}

/// `castAsStringFunctionClass`.
#[derive(Clone, Debug)]
pub struct CastAsStringFunctionClass {
    /// The embedded `baseFunctionClass`.
    pub base: BaseFunctionClass,
    /// `tp`.
    pub tp: FieldType,
    /// `isExplicitCharset`.
    pub is_explicit_charset: bool,
}

impl CastAsStringFunctionClass {
    /// Go changes the class's `tp` in place (`adjustRetFtForCastString(bf.tp, ..)` on the
    /// pointer the caller made the node's `RetType`); the result type is the signature's `tp`
    /// here, which the caller uses as the node's `RetType`.
    // go: castAsStringFunctionClass.getFunction L1
    pub fn get_function(
        &self,
        ctx: &dyn BuildContext,
        args: Vec<ExprRef>,
    ) -> Result<BuiltinFunc, Error> {
        self.base.verify_args(&args)?;
        let mut bf = new_base_builtin_cast_func4_string(
            ctx,
            self.base.func_name,
            args,
            self.tp.clone(),
            self.is_explicit_charset,
        )?;
        let ft = bf.args[0].get_type(&*ctx.get_eval_ctx()).into_owned();
        if ft.hybrid() {
            let cast_bit_as_un_binary =
                ft.get_type() == TypeBit && self.tp.get_charset() != CharsetBin;
            if !cast_bit_as_un_binary {
                return Ok(bf.with_sig(&builtinCastStringAsStringSig));
            }
            // for type BIT, it maybe an invalid value for the specified charset, we need to
            // convert it to binary first, and then convert it to the specified charset with
            // `HandleBinaryLiteral` in the following code.
            let mut tp = tidb_types::field_type::new_field_type(TypeString);
            tp.set_charset(CharsetBin);
            tp.set_collate(CollationBin);
            tp.add_flag(BinaryFlag);
            let a = ExprRef::clone(&bf.args[0]);
            bf.args[0] = build_cast_function(ctx, a, tp)?;
        }
        let arg_ft = bf.args[0].get_type(&*ctx.get_eval_ctx()).into_owned();
        adjust_ret_ft_for_cast_string(&mut bf.tp, &arg_ft);

        let arg_tp = arg_ft.eval_type();
        Ok(match arg_tp {
            ETInt => bf.with_sig(&builtinCastIntAsStringSig),
            ETReal => bf.with_sig(&builtinCastRealAsStringSig),
            ETDecimal => bf.with_sig(&builtinCastDecimalAsStringSig),
            ETDatetime | ETTimestamp => bf.with_sig(&builtinCastTimeAsStringSig),
            ETDuration => bf.with_sig(&builtinCastDurationAsStringSig),
            ETJson => bf.with_sig(&builtinCastJSONAsStringSig),
            ETVectorFloat32 => bf.with_sig(&builtinCastVectorFloat32AsStringSig),
            ETString => {
                // When cast from binary to some other charsets, we should check if the binary
                // is valid or not. so we build a from_binary function to do this check.
                let ec = ExprCollation {
                    coer: CoercibilityExplicit,
                    repe: Default::default(),
                    charset: self.tp.get_charset().to_string(),
                    collation: self.tp.get_collate().to_string(),
                };
                let a = ExprRef::clone(&bf.args[0]);
                bf.args[0] = handle_binary_literal(ctx, a, &ec, self.base.func_name, true);
                bf.with_sig(&builtinCastStringAsStringSig)
            }
            _ => return Err(cannot_cast(arg_tp, "String")),
        })
    }
}

const maxTinyBlobSize: isize = 255;
const maxBlobSize: isize = 65535;
const maxMediumBlobSize: isize = 16777215;
const maxLongBlobSize: isize = 4294967295;
// These two are used to estimate the length of the result of CAST(.. AS CHAR) of a BLOB
// (4 bytes per character at most).
const castBlobFlen: isize = maxBlobSize * 4;
const castMediumBlobFlen: isize = maxMediumBlobSize * 4;

/// The flen of a CAST to a variable-length string type without a length, estimated from the
/// argument. Go's `intest.Assert` branches (ENUM/SET with `EnumSetAsIntFlag`, unknown types)
/// return as Go's production build does (D08.1 spec §4 item 6).
// go: adjustRetFtForCastString L1
#[allow(clippy::collapsible_match)] // keeps the shape of the Go switch
pub(crate) fn adjust_ret_ft_for_cast_string(ret_ft: &mut FieldType, arg_ft: &FieldType) {
    let original_flen = ret_ft.get_flen();

    // Only estimate the length for variable length string types, because different length
    // for fixed length string types will have different behaviors and may cause
    // compatibility issues.
    if ret_ft.get_type() == TypeString {
        return;
    }

    if arg_ft.get_type() == TypeNull {
        return;
    }

    let arg_tp = arg_ft.eval_type();
    let unsigned = has_unsigned_flag(arg_ft.get_flag());
    match arg_tp {
        ETInt => {
            if original_flen == UnspecifiedLength {
                // check https://github.com/pingcap/tidb/issues/44786
                // set flen from integers may truncate integers, e.g. char(1) can not display
                // -1[int(1)]
                match arg_ft.get_type() {
                    TypeTiny => ret_ft.set_flen(if unsigned { 3 } else { 4 }),
                    mysql::TypeShort => ret_ft.set_flen(if unsigned { 5 } else { 6 }),
                    TypeInt24 => ret_ft.set_flen(if unsigned { 8 } else { 9 }),
                    TypeLong => ret_ft.set_flen(if unsigned { 10 } else { 11 }),
                    // the length of BIGINT is always 20 without considering the unsigned flag,
                    // because the bigint range from -9223372036854775808 to
                    // 9223372036854775807, and unsigned bigint range from 0 to
                    // 18446744073709551615, they are all 20 characters long.
                    TypeLonglong => ret_ft.set_flen(20),
                    TypeYear => ret_ft.set_flen(4),
                    TypeBit => ret_ft.set_flen(arg_ft.get_flen()),
                    // intest.Assert(false, ..): cast Enum/Set to String should not set
                    // mysql.EnumSetAsIntFlag; unknown type for INT.
                    TypeEnum | TypeSet => {}
                    _ => {}
                }
            }
        }
        ETReal => {
            // MySQL used 12/22 for float/double, it's because MySQL turns float/double into
            // scientific notation in some situations. TiDB choose to use 'f' format for all
            // the cases, so TiDB needs much longer length for float/double.
            //
            // The largest float/double value is around `3.40e38`/`1.79e308`, and the smallest
            // positive float/double value is around `1.40e-45`/`4.94e-324`. Therefore, we need
            // at least `1 (sign) + 1 (integer) + 1 (dot) + (45 + 39) (decimal) = 87` for float
            // and `1 (sign) + 1 (integer) + 1 (dot) + (324 + 43) (decimal) = 370` for double.
            if original_flen == UnspecifiedLength {
                if arg_ft.get_type() == TypeFloat {
                    ret_ft.set_flen(87);
                } else if arg_ft.get_type() == TypeDouble {
                    ret_ft.set_flen(370);
                }
            }
        }
        ETDecimal => {
            if original_flen == UnspecifiedLength {
                ret_ft.set_flen(decimal_precision_to_length(arg_ft));
            }
        }
        ETDatetime | ETTimestamp => {
            if original_flen == UnspecifiedLength {
                if arg_ft.get_type() == TypeDate {
                    ret_ft.set_flen(MaxDateWidth);
                } else {
                    ret_ft.set_flen(MaxDatetimeWidthNoFsp);
                }

                // Theoretically, the decimal of `DATE` will never be greater than 0.
                let decimal = arg_ft.get_decimal();
                if decimal > 0 {
                    // If the type is datetime or timestamp with fractional seconds, we need to
                    // set the length to accommodate the fractional seconds part.
                    ret_ft.set_flen(ret_ft.get_flen() + 1 + decimal);
                }
            }
        }
        ETDuration => {
            if original_flen == UnspecifiedLength {
                ret_ft.set_flen(MaxDurationWidthNoFsp);
                let decimal = arg_ft.get_decimal();
                if decimal > 0 {
                    // If the type is time with fractional seconds, we need to set the length
                    // to accommodate the fractional seconds part.
                    ret_ft.set_flen(ret_ft.get_flen() + 1 + decimal);
                }
            }
        }
        ETJson => {
            if original_flen == UnspecifiedLength {
                ret_ft.set_flen(MaxLongBlobWidth);
                ret_ft.set_type(TypeLongBlob);
            }
        }
        ETVectorFloat32 => {}
        ETString => {
            if original_flen == UnspecifiedLength {
                match arg_ft.get_type() {
                    TypeString | TypeVarchar | TypeVarString => {
                        if arg_ft.get_flen() > 0 {
                            ret_ft.set_flen(arg_ft.get_flen());
                        }
                    }
                    TypeTinyBlob => ret_ft.set_flen(maxTinyBlobSize),
                    TypeBlob => ret_ft.set_flen(castBlobFlen),
                    TypeMediumBlob => ret_ft.set_flen(castMediumBlobFlen),
                    TypeLongBlob => ret_ft.set_flen(maxLongBlobSize),
                    // intest.Assert(false, "unknown type %d for String")
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

/// The display length of a DECIMAL(precision, scale): digits, the point, the sign.
// go: decimalPrecisionToLength L1
fn decimal_precision_to_length(ft: &FieldType) -> isize {
    let precision = ft.get_flen();
    let scale = ft.get_decimal();
    let unsigned = has_unsigned_flag(ft.get_flag());

    if precision == UnspecifiedLength || scale == UnspecifiedLength {
        return UnspecifiedLength;
    }

    let mut ret = precision;
    if scale > 0 {
        ret += 1;
    }

    if !unsigned && precision > 0 {
        ret += 1; // for negative sign
    }

    if ret == 0 {
        return 1;
    }
    ret
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
        Expr::ScalarFunction(sf) => f(sf.ret_type_mut()),
    }
}

/// PropagateType change the type of the argument according to the evaluation type of the
/// function (only ETReal changes a DECIMAL argument's flen and decimal).
// go: pkg/expression/expression.go PropagateType L2 (a shared argument node is copied before its type changes, spec §4)
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

/// BuildCastFunctionWithCheck builds a CAST ScalarFunction from the Expression and return
/// error if any. Go returns the node with a nil signature together with an error; this
/// returns only the error.
// go: BuildCastFunctionWithCheck L2 (the error comes without the node)
pub fn build_cast_function_with_check(
    ctx: &dyn BuildContext,
    expr: ExprRef,
    mut tp: FieldType,
    in_union: bool,
    is_explicit_charset: bool,
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
        ETInt => CastAsIntFunctionClass { base, tp, in_union }.get_function(ctx, vec![expr])?,
        ETDecimal => {
            CastAsDecimalFunctionClass { base, tp, in_union }.get_function(ctx, vec![expr])?
        }
        ETReal => CastAsRealFunctionClass { base, tp, in_union }.get_function(ctx, vec![expr])?,
        ETString => {
            let arg_tp = expr.get_type(&*ectx).into_owned();
            if arg_tp.get_type() == TypeBit {
                tp.set_flen((arg_tp.get_flen() + 7) / 8);
            }
            drop(ectx);
            CastAsStringFunctionClass {
                base,
                tp,
                is_explicit_charset,
            }
            .get_function(ctx, vec![expr])?
        }
        ETDatetime | ETTimestamp => {
            drop(ectx);
            CastAsTimeFunctionClass { base, tp }.get_function(ctx, vec![expr])?
        }
        ETDuration => {
            drop(ectx);
            CastAsDurationFunctionClass { base, tp }.get_function(ctx, vec![expr])?
        }
        ETJson => {
            drop(ectx);
            if tp.is_array() {
                CastAsArrayFunctionClass { base, tp }.get_function(ctx, vec![expr])?
            } else {
                CastAsJSONFunctionClass { base, tp }.get_function(ctx, vec![expr])?
            }
        }
        ETVectorFloat32 => {
            drop(ectx);
            CastAsVectorFloat32FunctionClass { base, tp }.get_function(ctx, vec![expr])?
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

/// BuildCastCollationFunction builds a ScalarFunction which casts the collation. Go logs
/// the error of `BuildCastFunction`; this returns it.
// go: BuildCastCollationFunction L2 (returns the error BuildCastFunction logs)
pub fn build_cast_collation_function(
    ctx: &dyn BuildContext,
    expr: ExprRef,
    ec: &ExprCollation,
    enum_or_set_real_type_is_str: bool,
) -> Result<ExprRef, Error> {
    let ectx = ctx.get_eval_ctx();
    let et = expr.get_type(&*ectx).into_owned();
    if et.eval_type() != ETString {
        return Ok(expr);
    }
    if et.get_collate() == ec.collation {
        return Ok(expr);
    }
    let mut tp = et.clone();
    if et.hybrid() {
        if !enum_or_set_real_type_is_str {
            return Ok(expr);
        }
        tp = tidb_types::field_type::new_field_type(TypeVarString);
    } else if ec.charset == CharsetBin {
        // When cast character string to binary string, if we still use fixed length
        // representation, then 0 padding will be used, which can affect later execution.
        // e.g. https://github.com/pingcap/tidb/issues/34823.
        // On the other hand, we can not directly return origin expr back, since we need
        // binary collation to do string comparison later.
        // e.g. https://github.com/pingcap/tidb/pull/35053#discussion_r894155052
        // Here we use VarString type of cast, i.e `cast(a as binary)`, to avoid this problem.
        tp.set_type(TypeVarString);
    }
    tp.set_charset(&ec.charset);
    tp.set_collate(&ec.collation);
    drop(ectx);
    build_cast_function(ctx, expr, tp)
}

/// WrapWithCastAsString wraps `expr` with `cast` if the return type of expr is not type
/// string, otherwise, returns `expr` directly.
// go: WrapWithCastAsString L2 (returns the error BuildCastFunction logs)
pub fn wrap_with_cast_as_string(ctx: &dyn BuildContext, expr: ExprRef) -> Result<ExprRef, Error> {
    let ectx = ctx.get_eval_ctx();
    let expr_tp = expr.get_type(&*ectx).into_owned();
    if expr_tp.eval_type() == ETString {
        return Ok(expr);
    }
    let mut arg_len = expr_tp.get_flen();
    // If expr is decimal, we should take the decimal point ,negative sign and the leading
    // zero(0.xxx) into consideration, so we set `expr.GetType(ctx.GetEvalCtx()).GetFlen() + 3`
    // as the `argLen`. Since the length of float and double is not accurate, we do not handle
    // them.
    if expr_tp.get_type() == TypeNewDecimal && arg_len != tidb_types::fsp::UnspecifiedFsp {
        arg_len += 3;
    }

    if expr_tp.eval_type() == ETInt {
        arg_len = MaxIntWidth;
        // For TypeBit, castAsString will make length as int(( bit_len + 7 ) / 8) bytes due to
        // TiKV needs the bit's real len during calculating, eg: ascii(bit).
        if expr_tp.get_type() == TypeBit {
            arg_len = (expr_tp.get_flen() + 7) / 8;
        }
    }

    // Because we can't control the length of cast(float as char) for now, we can't
    // determine the argLen.
    if expr_tp.get_type() == TypeFloat || expr_tp.get_type() == TypeDouble {
        arg_len = -1;
    }
    let mut tp = tidb_types::field_type::new_field_type(TypeVarString);
    if expr.coercibility() == CoercibilityExplicit {
        let (charset, collate) = expr.charset_and_collation();
        tp.set_charset(charset);
        tp.set_collate(collate);
    } else if expr_tp.get_type() == TypeBit {
        // Implicitly casting BIT to string will make it a binary
        tp.set_charset(CharsetBin);
        tp.set_collate(CollationBin);
    } else {
        let (charset, collate) = ctx.get_charset_info();
        tp.set_charset(&charset);
        tp.set_collate(&collate);
    }
    tp.set_flen(arg_len);
    tp.set_decimal(UnspecifiedLength);
    drop(ectx);
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
pub fn wrap_with_cast_as_decimal(ctx: &dyn BuildContext, expr: ExprRef) -> Result<ExprRef, Error> {
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

/// WrapWithCastAsTime wraps `expr` with `cast` if the return type of expr is not same as
/// type of the specified `tp` , otherwise, returns `expr` directly. Go changes the caller's
/// `tp` in place; here `tp` is taken by value (spec §4).
// go: WrapWithCastAsTime L2 (tp by value; returns the error BuildCastFunction logs)
pub fn wrap_with_cast_as_time(
    ctx: &dyn BuildContext,
    expr: ExprRef,
    mut tp: FieldType,
) -> Result<ExprRef, Error> {
    let ectx = ctx.get_eval_ctx();
    let et = expr.get_type(&*ectx).into_owned();
    drop(ectx);
    let expr_tp = et.get_type();
    if tp.get_type() == expr_tp
        || ((expr_tp == TypeDate || expr_tp == TypeTimestamp) && tp.get_type() == TypeDatetime)
    {
        return Ok(expr);
    }
    match et.eval_type() {
        ETInt => tp.set_decimal(MinFsp),
        ETString | ETReal | ETJson => tp.set_decimal(MaxFsp),
        ETDatetime | ETTimestamp | ETDuration => tp.set_decimal(et.get_decimal()),
        ETDecimal => {
            tp.set_decimal(et.get_decimal());
            if tp.get_decimal() > MaxFsp {
                tp.set_decimal(MaxFsp);
            }
        }
        _ => {}
    }
    match tp.get_type() {
        TypeDate => tp.set_flen(MaxDateWidth),
        TypeDatetime | TypeTimestamp => {
            tp.set_flen(MaxDatetimeWidthNoFsp);
            if tp.get_decimal() > 0 {
                tp.set_flen(tp.get_flen() + 1 + tp.get_decimal());
            }
        }
        _ => {}
    }
    set_bin_chs_cln_flag(&mut tp);
    build_cast_function(ctx, expr, tp)
}

/// WrapWithCastAsDuration wraps `expr` with `cast` if the return type of expr is not type
/// duration, otherwise, returns `expr` directly.
// go: WrapWithCastAsDuration L2 (returns the error BuildCastFunction logs)
pub fn wrap_with_cast_as_duration(ctx: &dyn BuildContext, expr: ExprRef) -> Result<ExprRef, Error> {
    let ectx = ctx.get_eval_ctx();
    let x = expr.get_type(&*ectx).into_owned();
    drop(ectx);
    if x.get_type() == TypeDuration {
        return Ok(expr);
    }
    let mut tp = new_field_type(TypeDuration);
    match x.get_type() {
        TypeDatetime | TypeTimestamp | TypeDate => tp.set_decimal(x.get_decimal()),
        _ => tp.set_decimal(MaxFsp),
    }
    tp.set_flen(MaxDurationWidthNoFsp);
    if tp.get_decimal() > 0 {
        tp.set_flen(tp.get_flen() + 1 + tp.get_decimal());
    }
    build_cast_function(ctx, expr, tp)
}

/// WrapWithCastAsJSON wraps `expr` with `cast` if the return type of expr is not type json,
/// otherwise, returns `expr` directly.
// go: WrapWithCastAsJSON L2 (returns the error BuildCastFunction logs)
pub fn wrap_with_cast_as_json(ctx: &dyn BuildContext, expr: ExprRef) -> Result<ExprRef, Error> {
    let ectx = ctx.get_eval_ctx();
    let x = expr.get_type(&*ectx).into_owned();
    drop(ectx);
    if x.get_type() == TypeJSON && !has_parse_to_json_flag(x.get_flag()) {
        return Ok(expr);
    }
    // types.NewFieldTypeBuilder() starts from the zero FieldType (decimal 0).
    let mut tp = new_field_type(TypeJSON);
    tp.set_decimal(0);
    tp.set_flag(BinaryFlag);
    tp.set_flen(12582912);
    tp.set_charset(DefaultCharset);
    tp.set_collate(DefaultCollationName);
    build_cast_function(ctx, expr, tp)
}

/// WrapWithCastAsVectorFloat32 wraps `expr` with `cast` if the return type of expr is not
/// type VectorFloat32, otherwise, returns `expr` directly.
// go: WrapWithCastAsVectorFloat32 L2 (returns the error BuildCastFunction logs)
pub fn wrap_with_cast_as_vector_float32(
    ctx: &dyn BuildContext,
    expr: ExprRef,
) -> Result<ExprRef, Error> {
    if expr.get_type(&*ctx.get_eval_ctx()).get_type() == TypeTiDBVectorFloat32 {
        return Ok(expr);
    }
    build_cast_function(ctx, expr, new_field_type(TypeTiDBVectorFloat32))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn float_length_matches_go() {
        assert_eq!(
            set_data_type_double(2),
            (23 + 2 - 8 + NotFixedDec, NotFixedDec)
        );
        assert_eq!(float_length(NotFixedDec, 0), 23);
    }
}
