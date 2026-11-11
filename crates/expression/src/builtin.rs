//! go-file: pkg/expression/builtin.go
//!
//! Block D08.2: building a signature's `baseBuiltinFunc` (result type, collation, NOT NULL
//! flag), the function classes' argument count check, and the per-context cache of a
//! signature. The `baseBuiltinFunc` fields and methods a built node uses are in
//! [`crate::eval::BuiltinFunc`]; the `funcs` table is in [`crate::builtin_registry`].
//!
//! `newBaseBuiltinFuncWithTp` wraps the arguments in CAST (`WrapWithCastAs*`, block D08.8).
//!
//! Not here yet (spec `specs/D08.2-funcs` §4):
//! - `newBaseBuiltinFuncWithFieldTypes` (with the CAST targets of D08.8 part 2b-2);
//! - `baseBuiltinFunc.vecEvalXxx` are replaced by the vector drivers (M-EVAL).

#![allow(non_upper_case_globals)]

use std::sync::Mutex;

use tidb_base::{Error, mysql};
use tidb_parser::gocharset::{CharsetBin, CollationBin};
use tidb_parser::types::UnspecifiedLength;
use tidb_parser::types::{
    ETDatetime, ETDecimal, ETDuration, ETInt, ETJson, ETReal, ETString, ETTimestamp,
    ETVectorFloat32, EvalType,
};
use tidb_types::collate;
use tidb_types::etc::FieldType;
use tidb_types::field_type::new_field_type;
use tidb_types::field_type_builder::new_field_type_builder;
use tidb_types::fsp::MaxFsp;

use crate::ast_funcs as ast;
use crate::builtin_cast::{
    build_cast_function, wrap_with_cast_as_decimal, wrap_with_cast_as_duration,
    wrap_with_cast_as_int, wrap_with_cast_as_json, wrap_with_cast_as_real,
    wrap_with_cast_as_string, wrap_with_cast_as_time, wrap_with_cast_as_vector_float32,
};
use crate::builtin_convert_charset::handle_binary_literal;
use crate::collation::{ASCII, CoercibilityExplicit, ExprCollation, UNICODE, derive_collation};
use crate::context::{BuildContext, EvalContext, OptionalEvalPropKeySet};
use crate::errors::ErrIncorrectParameterCount;
use crate::eval::{BuiltinFunc, SigDesc, SigDrivers};
use crate::expression::ExprRef;
use crate::function_traits::{booleanFunctions, in_set};

/// The placeholder signature of a `baseBuiltinFunc` that is not yet embedded in a concrete
/// signature (Go's `baseBuiltinFunc` value returned by `newBaseBuiltinFunc`); the function
/// class then calls [`BuiltinFunc::with_sig`].
pub static baseBuiltinFuncSig: SigDesc = SigDesc {
    name: "baseBuiltinFunc",
    pb_code: 0,
    vectorized: false,
    drivers: SigDrivers::None,
};

/// `tipb.InUnionMetadata`: the metadata of a CAST signature.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InUnionMetadata {
    /// `InUnion`.
    pub in_union: bool,
}

/// `newBaseBuiltinCastFunc`: a `baseBuiltinCastFunc` over `builtin_func` (the base of the CAST
/// signatures that read `inUnion`).
// go: newBaseBuiltinCastFunc L1
pub fn new_base_builtin_cast_func(builtin_func: BuiltinFunc, in_union: bool) -> BuiltinFunc {
    let mut bf = builtin_func;
    bf.extra = crate::eval::SigExtra::Cast { in_union };
    bf
}

/// baseFunctionClass will be contained in every struct that implement functionClass interface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BaseFunctionClass {
    /// funcName.
    pub func_name: &'static str,
    /// minArgs.
    pub min_args: i32,
    /// maxArgs; -1 is unbounded.
    pub max_args: i32,
}

impl BaseFunctionClass {
    // go: baseFunctionClass.verifyArgs L1
    pub fn verify_args(&self, args: &[ExprRef]) -> Result<(), Error> {
        self.verify_args_by_count(args.len())
    }

    // go: baseFunctionClass.verifyArgsByCount L1
    pub fn verify_args_by_count(&self, l: usize) -> Result<(), Error> {
        let l = l as i64;
        if l < self.min_args as i64 || (self.max_args != -1 && l > self.max_args as i64) {
            return Err(
                ErrIncorrectParameterCount.gen_with_stack_by_args(vec![self.func_name.into()])
            );
        }
        Ok(())
    }
}

/// functions that always return not null type
pub(crate) static functionSetForReturnTypeAlwaysNotNull: &[&str] = &[ast::IsNull, ast::NullEQ];

/// functions that always return nullable type
pub(crate) static functionSetForReturnTypeAlwaysNullable: &[&str] = &[
    ast::Div,    /* divide by zero */
    ast::IntDiv, /* divide by zero */
    ast::Mod,    /* mod by zero */
];

/// functions that if all the inputs are not null, then the result is not null
pub(crate) static functionSetForReturnTypeNotNullOnNotNull: &[&str] =
    &[ast::Plus, ast::Minus, ast::Mul];

/// Go passes the `baseBuiltinFunc` by value, but its `tp` is a pointer: only the result
/// type changes, so the result type is what this takes.
// go: adjustNullFlagForReturnType L1
pub(crate) fn adjust_null_flag_for_return_type(
    ctx: &dyn EvalContext,
    func_name: &str,
    args: &[ExprRef],
    tp: &mut FieldType,
) {
    if in_set(functionSetForReturnTypeAlwaysNotNull, func_name) {
        tp.add_flag(mysql::NotNullFlag);
    } else if in_set(functionSetForReturnTypeAlwaysNullable, func_name) {
        tp.del_flag(mysql::NotNullFlag);
    } else if in_set(functionSetForReturnTypeNotNullOnNotNull, func_name) {
        let mut return_nullable = false;
        for arg in args {
            if !mysql::has_not_null_flag(arg.get_type(ctx).get_flag()) {
                return_nullable = true;
                break;
            }
        }
        if return_nullable {
            tp.del_flag(mysql::NotNullFlag);
        } else {
            tp.add_flag(mysql::NotNullFlag);
        }
    }
}

/// Sets the derived collation on a new base.
// glue: the four Set* calls shared by the newBaseBuiltinFunc* constructors
fn set_collation(bf: &mut BuiltinFunc, ec: &ExprCollation) {
    bf.collation_info
        .set_charset_and_collation(&ec.charset, &ec.collation);
    bf.set_collator(collate::get_collator(&ec.collation));
    bf.collation_info.set_coercibility(ec.coer);
    bf.collation_info.set_repertoire(ec.repe);
}

/// `newBaseBuiltinFunc`: the base of a signature whose result type the caller gives. Go
/// returns an error for a nil context, which a reference cannot be.
// go: newBaseBuiltinFunc L2 (no nil context)
pub fn new_base_builtin_func(
    ctx: &dyn BuildContext,
    func_name: &str,
    args: Vec<ExprRef>,
    tp: FieldType,
) -> Result<BuiltinFunc, Error> {
    let ret_type = tp.eval_type();
    let ec = derive_collation(ctx, func_name, &args, ret_type, &[ret_type])?;

    let mut bf = BuiltinFunc::base(args, tp);
    set_collation(&mut bf, &ec);
    adjust_null_flag_for_return_type(&*ctx.get_eval_ctx(), func_name, &bf.args, &mut bf.tp);
    Ok(bf)
}

/// newBaseBuiltinFuncWithTp creates a built-in function signature with specified types of
/// arguments and the return type of the function. `arg_tps` indicates the types of the args,
/// `ret_type` indicates the return type of the built-in function. Every built-in function
/// needs to be determined argTps and retType when we create it.
///
/// Go returns an error for a nil context, which a reference cannot be, and panics when the
/// lengths differ (so does this). A CAST wrapper's build error, which Go's `WrapWithCastAs*`
/// logs, is returned.
// go: newBaseBuiltinFuncWithTp L2 (no nil context; returns the error WrapWithCastAs* logs)
pub fn new_base_builtin_func_with_tp(
    ctx: &dyn BuildContext,
    func_name: &str,
    args: Vec<ExprRef>,
    ret_type: EvalType,
    arg_tps: &[EvalType],
) -> Result<BuiltinFunc, Error> {
    if args.len() != arg_tps.len() {
        panic!("unexpected length of args and argTps");
    }

    // derive collation information for string function, and we must do it
    // before doing implicit cast.
    let ec = derive_collation(ctx, func_name, &args, ret_type, arg_tps)?;

    let mut wrapped = Vec::with_capacity(args.len());
    for (a, tp) in args.into_iter().zip(arg_tps) {
        wrapped.push(match *tp {
            ETInt => wrap_with_cast_as_int(ctx, a)?,
            ETReal => wrap_with_cast_as_real(ctx, a)?,
            ETDecimal => wrap_with_cast_as_decimal(ctx, a)?,
            ETString => {
                let a = wrap_with_cast_as_string(ctx, a)?;
                handle_binary_literal(ctx, a, &ec, func_name, false)
            }
            ETDatetime => wrap_with_cast_as_time(ctx, a, new_field_type(mysql::TypeDatetime))?,
            ETTimestamp => wrap_with_cast_as_time(ctx, a, new_field_type(mysql::TypeTimestamp))?,
            ETDuration => wrap_with_cast_as_duration(ctx, a)?,
            ETJson => wrap_with_cast_as_json(ctx, a)?,
            ETVectorFloat32 => wrap_with_cast_as_vector_float32(ctx, a)?,
            _ => return Err(Error::plain(format!("{} is not supported", tp.to_string()))),
        });
    }

    let field_type = new_return_field_type_for_base_builtin_func(func_name, ret_type, &ec);
    let mut bf = BuiltinFunc::base(wrapped, field_type);
    set_collation(&mut bf, &ec);
    // note this function must be called after wrap cast function to the args
    adjust_null_flag_for_return_type(&*ctx.get_eval_ctx(), func_name, &bf.args, &mut bf.tp);
    Ok(bf)
}

/// newBaseBuiltinFuncWithFieldTypes is very similar to [`new_base_builtin_func_with_tp`],
/// the only difference is that it takes field types instead of evaluation types of the
/// arguments: a DECIMAL/DATETIME/TIMESTAMP/DURATION argument whose type differs from the
/// given one is wrapped in a CAST to that exact type (pingcap/tidb#44196), so the decimal
/// and fsp match the declared argument type.
///
/// Go returns an error for a nil context, which a reference cannot be, and panics when the
/// lengths differ (so does this). A CAST wrapper's build error, which Go's `WrapWithCastAs*`
/// and `BuildCastFunction` log, is returned. Go has no case for VECTOR arguments: they are
/// passed through unchanged here too.
// go: newBaseBuiltinFuncWithFieldTypes L2 (no nil context; returns the errors the wrappers log)
pub fn new_base_builtin_func_with_field_types(
    ctx: &dyn BuildContext,
    func_name: &str,
    args: Vec<ExprRef>,
    ret_type: EvalType,
    arg_tps: &[&FieldType],
) -> Result<BuiltinFunc, Error> {
    if args.len() != arg_tps.len() {
        panic!("unexpected length of args and argTps");
    }

    // derive collation information for string function, and we must do it
    // before doing implicit cast.
    let arg_eval_tps: Vec<EvalType> = arg_tps.iter().map(|t| t.eval_type()).collect();
    let ec = derive_collation(ctx, func_name, &args, ret_type, &arg_eval_tps)?;

    let mut wrapped = Vec::with_capacity(args.len());
    for (a, tp) in args.into_iter().zip(arg_tps) {
        wrapped.push(match tp.eval_type() {
            ETInt => wrap_with_cast_as_int(ctx, a)?,
            ETReal => wrap_with_cast_as_real(ctx, a)?,
            ETString => {
                let a = wrap_with_cast_as_string(ctx, a)?;
                handle_binary_literal(ctx, a, &ec, func_name, false)
            }
            ETJson => wrap_with_cast_as_json(ctx, a)?,
            // https://github.com/pingcap/tidb/issues/44196
            // For decimal/datetime/timestamp/duration types, it is necessary to ensure that
            // decimal are consistent with the output type, so adding a cast function here.
            ETDecimal | ETDatetime | ETTimestamp | ETDuration => {
                if !a.get_type(&*ctx.get_eval_ctx()).equal(tp) {
                    build_cast_function(ctx, a, (*tp).clone())?
                } else {
                    a
                }
            }
            _ => a,
        });
    }

    let field_type = new_return_field_type_for_base_builtin_func(func_name, ret_type, &ec);
    let mut bf = BuiltinFunc::base(wrapped, field_type);
    set_collation(&mut bf, &ec);
    // note this function must be called after wrap cast function to the args
    adjust_null_flag_for_return_type(&*ctx.get_eval_ctx(), func_name, &bf.args, &mut bf.tp);
    Ok(bf)
}

/// `newBaseBuiltinCastFunc4String`: the base of CAST to a string type. With an explicit
/// charset (`CAST(.. AS CHAR CHARACTER SET ..)`) the collation comes from the target type and
/// is explicit; otherwise as `newBaseBuiltinFunc`.
// go: newBaseBuiltinCastFunc4String L1
pub fn new_base_builtin_cast_func4_string(
    ctx: &dyn BuildContext,
    func_name: &str,
    args: Vec<ExprRef>,
    tp: FieldType,
    is_explicit_charset: bool,
) -> Result<BuiltinFunc, Error> {
    if is_explicit_charset {
        let (charset, collate) = (tp.get_charset().to_string(), tp.get_collate().to_string());
        let mut bf = BuiltinFunc::base(args, tp);
        bf.collation_info
            .set_charset_and_collation(&charset, &collate);
        bf.set_collator(collate::get_collator(&collate));
        bf.collation_info.set_coercibility(CoercibilityExplicit);
        bf.collation_info.set_explicit_charset(true);
        if charset == tidb_parser::gocharset::CharsetASCII {
            bf.collation_info.set_repertoire(ASCII);
        } else {
            bf.collation_info.set_repertoire(UNICODE);
        }
        return Ok(bf);
    }
    new_base_builtin_func(ctx, func_name, args, tp)
}

/// The result type a signature of evaluation type `ret_type` starts with.
// go: newReturnFieldTypeForBaseBuiltinFunc L1
pub fn new_return_field_type_for_base_builtin_func(
    func_name: &str,
    ret_type: EvalType,
    ec: &ExprCollation,
) -> FieldType {
    let b = new_field_type_builder();
    let mut field_type = match ret_type {
        ETInt => b
            .set_type(mysql::TypeLonglong)
            .set_flag(mysql::BinaryFlag)
            .set_flen(mysql::MaxIntWidth)
            .build_p(),
        ETReal => b
            .set_type(mysql::TypeDouble)
            .set_flag(mysql::BinaryFlag)
            .set_flen(mysql::MaxRealWidth)
            .set_decimal(UnspecifiedLength)
            .build_p(),
        ETDecimal => b
            .set_type(mysql::TypeNewDecimal)
            .set_flag(mysql::BinaryFlag)
            .set_flen(11)
            .build_p(),
        ETString => b
            .set_type(mysql::TypeVarString)
            .set_flen(UnspecifiedLength)
            .set_decimal(UnspecifiedLength)
            .set_charset(&ec.charset)
            .set_collate(&ec.collation)
            .build_p(),
        ETDatetime => b
            .set_type(mysql::TypeDatetime)
            .set_flag(mysql::BinaryFlag)
            .set_flen(mysql::MaxDatetimeWidthWithFsp)
            .set_decimal(MaxFsp)
            .build_p(),
        ETTimestamp => b
            .set_type(mysql::TypeTimestamp)
            .set_flag(mysql::BinaryFlag)
            .set_flen(mysql::MaxDatetimeWidthWithFsp)
            .set_decimal(MaxFsp)
            .build_p(),
        ETDuration => b
            .set_type(mysql::TypeDuration)
            .set_flag(mysql::BinaryFlag)
            .set_flen(mysql::MaxDurationWidthWithFsp)
            .set_decimal(MaxFsp)
            .build_p(),
        ETJson => b
            .set_type(mysql::TypeJSON)
            .set_flag(mysql::BinaryFlag)
            .set_flen(mysql::MaxBlobWidth)
            .set_charset(mysql::DefaultCharset)
            .set_collate(mysql::DefaultCollationName)
            .build_p(),
        ETVectorFloat32 => b
            .set_type(mysql::TypeTiDBVectorFloat32)
            .set_flag(mysql::BinaryFlag)
            .set_flen(UnspecifiedLength)
            .build_p(),
        // Go leaves fieldType nil and dereferences it below.
        _ => panic!("runtime error: invalid memory address or nil pointer dereference"),
    };
    if mysql::has_binary_flag(field_type.get_flag()) && field_type.get_type() != mysql::TypeJSON {
        field_type.set_charset(CharsetBin);
        field_type.set_collate(CollationBin);
    }
    if in_set(booleanFunctions, func_name) {
        field_type.add_flag(mysql::IsBooleanFlag);
    }
    field_type
}

/// newBaseBuiltinFuncWithFieldType create BaseBuiltinFunc with FieldType charset and
/// collation. do not check and compute collation.
// go: newBaseBuiltinFuncWithFieldType L1
pub fn new_base_builtin_func_with_field_type(
    tp: FieldType,
    args: Vec<ExprRef>,
) -> Result<BuiltinFunc, Error> {
    let (charset, collate) = (tp.get_charset().to_string(), tp.get_collate().to_string());
    let mut bf = BuiltinFunc::base(args, tp);
    bf.collation_info
        .set_charset_and_collation(&charset, &collate);
    bf.set_collator(collate::get_collator(&collate));
    Ok(bf)
}

impl BuiltinFunc {
    // go: baseBuiltinFunc.RequiredOptionalEvalProps L1
    pub fn required_optional_eval_props(&self) -> OptionalEvalPropKeySet {
        OptionalEvalPropKeySet::default()
    }

    /// metadata returns the metadata of a function: the inner fields that are not in
    /// `tipb.Expr.children` but must be pushed down. Only the CAST signatures that embed
    /// `baseBuiltinCastFunc` have some (`in_union` is `Some`); `None` is Go's nil
    /// `proto.Message`.
    // go: baseBuiltinFunc.metadata L1
    // go: baseBuiltinCastFunc.metadata L1
    pub fn metadata(&self) -> Option<InUnionMetadata> {
        self.in_union().map(|in_union| InUnionMetadata { in_union })
    }

    // go: baseBuiltinFunc.setPbCode L1
    pub fn set_pb_code(&mut self, c: i32) {
        self.pb_code = c;
    }

    // go: baseBuiltinFunc.setDecimalAndFlenForDatetime L1
    pub fn set_decimal_and_flen_for_datetime(&mut self, fsp: i32) {
        self.tp.set_decimal_under_limit(fsp as isize);
        self.tp
            .set_flen_under_limit(mysql::MaxDatetimeWidthNoFsp + fsp as isize);
        if fsp > 0 {
            // Add the length for `.`.
            let flen = self.tp.get_flen();
            self.tp.set_flen_under_limit(flen + 1);
        }
    }

    // go: baseBuiltinFunc.setDecimalAndFlenForDate L1
    pub fn set_decimal_and_flen_for_date(&mut self) {
        self.tp.set_decimal(0);
        self.tp.set_flen(mysql::MaxDateWidth);
        self.tp.set_type(mysql::TypeDate);
    }

    // go: baseBuiltinFunc.setDecimalAndFlenForTime L1
    pub fn set_decimal_and_flen_for_time(&mut self, fsp: i32) {
        self.tp.set_decimal_under_limit(fsp as isize);
        self.tp
            .set_flen_under_limit(mysql::MaxDurationWidthNoFsp + fsp as isize);
        if fsp > 0 {
            // Add the length for `.`.
            let flen = self.tp.get_flen();
            self.tp.set_flen_under_limit(flen + 1);
        }
    }
}

/// `builtinFuncCache[T]`: a value a signature computes once per evaluation context (for
/// example a compiled regexp for a constant pattern), keyed by `EvalContext.CtxID()`.
/// Go reads it through an `atomic.Pointer` and builds it under the mutex; this holds the
/// mutex for both, with the same results.
#[derive(Debug, Default)]
pub struct BuiltinFuncCache<T> {
    cached: Mutex<Option<(u64, T)>>,
}

impl<T: Clone> BuiltinFuncCache<T> {
    /// A new, empty cache.
    // glue: zero value of builtinFuncCache
    pub fn new() -> Self {
        BuiltinFuncCache {
            cached: Mutex::new(None),
        }
    }

    // go: builtinFuncCache.getCache L2 (mutex instead of atomic.Pointer)
    pub fn get_cache(&self, ctx_id: u64) -> Option<T> {
        match &*self.cached.lock().unwrap() {
            Some((id, item)) if *id == ctx_id => Some(item.clone()),
            _ => None,
        }
    }

    // go: builtinFuncCache.getOrInitCache L2 (mutex instead of atomic.Pointer)
    pub fn get_or_init_cache(
        &self,
        ctx: &dyn EvalContext,
        construct_cache: impl FnOnce() -> Result<T, Error>,
    ) -> Result<T, Error> {
        let ctx_id = ctx.ctx_id();
        let mut cached = self.cached.lock().unwrap();
        if let Some((id, item)) = &*cached
            && *id == ctx_id
        {
            return Ok(item.clone());
        }

        let item = construct_cache()?;
        *cached = Some((ctx_id, item.clone()));
        Ok(item)
    }
}

impl<T> Clone for BuiltinFuncCache<T> {
    /// The signatures holding a cache (LIKE, ILIKE, the REGEXP family, JSON_SCHEMA_VALID)
    /// build their clone as a new struct and copy only `baseBuiltinFunc` (`cloneFrom`), so
    /// the clone starts with an empty cache (spec §3.4, Lucky's review of #75).
    // glue: zero-value cache field of a signature's Clone
    fn clone(&self) -> Self {
        BuiltinFuncCache {
            cached: Mutex::new(None),
        }
    }
}
