//! go-file: pkg/expression/infer_pushdown.go
//!
//! Push-down eligibility (spec `specs/D08.3-pushdown` §3): which scalar functions TiKV, TiFlash
//! and TiDB evaluate, by function name and pb code, and the push-down blacklist
//! (`mysql.expr_pushdown_blacklist`).
//!
//! The TiFlash table and the TiFlash checks of `canExprPushDown` are ported in full although
//! TiFlash / MPP is outside the read-only phase (exception D1): `ExprToPB` asks whether *any*
//! store supports a function (`kv.UnSpecified`), so the TiFlash table decides what is pushed
//! to TiKV as well (spec §2).
//!
//! The test-only failpoints `PushDownTestSwitcher` and `PanicIfPbCodeUnspecified` are not
//! ported.

// Go identifiers for constants and statics (mapping rule M-GONAME).
#![allow(non_upper_case_globals)]

use std::collections::HashMap;
use std::sync::atomic::AtomicI64;
use std::sync::{Arc, LazyLock};

use arc_swap::ArcSwap;
use tidb_base::Error;
use tidb_base::contextutil::WarnAppenderRef;
use tidb_base::errors::RedactLogDisable;
use tidb_base::kv::{StoreType, TiDB, TiFlash, TiKV, UnSpecified};
use tidb_base::mysql;
use tidb_expression::ast_funcs as ast;
use tidb_expression::builtin_convert_charset::{InternalFuncFromBinary, InternalFuncToBinary};
use tidb_expression::{EvalContext, EvalState, Expr, ExprRef, ScalarFunction, is_binary_literal};
use tidb_kv::kv::Client;
use tidb_parser::gocharset::{CharsetBin, CollationBin};
use tidb_parser::types::{ETDecimal, ETInt, ETReal, type_str};
use tidb_sessionctx::exprctx::EvalContextRef;
use tidb_sessionctx::variable::session::SessionVars;

use crate::expr_to_pb::{PbConverter, empty_eval_state, new_pb_converter};
use crate::tipb_sig::{self as tipb, Unspecified, scalar_func_sig_string};

/// DefaultExprPushDownBlacklist indicates the expressions which can not be pushed down to TiKV.
pub static DefaultExprPushDownBlacklist: LazyLock<ArcSwap<HashMap<String, u32>>> =
    LazyLock::new(init);

/// ExprPushDownBlackListReloadTimeStamp is used to record the last time when the push-down
/// black list is reloaded. This is for plan cache, when the push-down black list is updated,
/// we invalid all cached plans to avoid error.
pub static ExprPushDownBlackListReloadTimeStamp: AtomicI64 = AtomicI64::new(0);

/// The failpoint `PushDownTestSwitcher` (integration tests only) is not ported.
// go: canFuncBePushed L2 (test-only failpoint not ported)
pub fn can_func_be_pushed(
    ctx: &dyn EvalContext,
    sf: &ScalarFunction,
    store_type: StoreType,
) -> bool {
    let mut ret = false;

    match store_type {
        TiFlash => ret = scalar_expr_supported_by_flash(ctx, sf),
        TiKV => ret = scalar_expr_supported_by_tikv(ctx, sf),
        TiDB => ret = scalar_expr_supported_by_tidb(ctx, sf),
        UnSpecified => {
            ret = scalar_expr_supported_by_tidb(ctx, sf)
                || scalar_expr_supported_by_tikv(ctx, sf)
                || scalar_expr_supported_by_flash(ctx, sf)
        }
        _ => {}
    }

    if ret {
        let func_full_name = format!(
            "{}.{}",
            sf.func_name.l,
            scalar_func_sig_string(sf.function.pb_code()).to_lowercase()
        );
        // Aside from checking function name, also check the pb name in case only the specific push down is disabled.
        ret = is_push_down_enabled(&sf.func_name.l, store_type)
            && is_push_down_enabled(&func_full_name, store_type);
    }
    ret
}

/// The failpoint `PanicIfPbCodeUnspecified` (tests only) is not ported. Go marshals the
/// metadata only to log a failure, which cannot happen for `InUnionMetadata`.
// go: canScalarFuncPushDown L2 (test-only failpoint not ported)
fn can_scalar_func_push_down(
    ctx: &PushDownContext<'_>,
    scalar_func: &ScalarFunction,
    store_type: StoreType,
) -> bool {
    let pb_code = scalar_func.function.pb_code();
    // Check whether this function can be pushed.
    let unspecified = pb_code <= Unspecified;
    if unspecified || !can_func_be_pushed(ctx.eval_ctx(), scalar_func, store_type) {
        let mut storage_name = store_type.name();
        if store_type == UnSpecified {
            storage_name = "storage layer";
        }
        let warn_err = Error::plain(format!(
            "Scalar function '{}'(signature: {}, return type: {}) is not supported to push down to {} now.",
            scalar_func.func_name.l,
            scalar_func_sig_string(pb_code),
            scalar_func.ret_type().compact_str(),
            storage_name
        ));

        ctx.append_warning(warn_err);
        return false;
    }
    let can_enum_push = can_enum_pushdown_preliminarily(scalar_func);
    // Check whether all of its parameters can be pushed.
    for arg in scalar_func.get_args() {
        if !can_expr_push_down(ctx, arg, store_type, can_enum_push) {
            return false;
        }
    }

    true
}

// go: canExprPushDown L1
fn can_expr_push_down(
    ctx: &PushDownContext<'_>,
    expr: &Expr,
    store_type: StoreType,
    can_enum_push: bool,
) -> bool {
    let pc = ctx.pb_converter();
    if store_type == TiFlash {
        let ft = expr.get_type(ctx.eval_ctx());
        match ft.get_type() {
            mysql::TypeEnum
            | mysql::TypeBit
            | mysql::TypeSet
            | mysql::TypeGeometry
            | mysql::TypeUnspecified => {
                if !(ft.get_type() == mysql::TypeEnum && can_enum_push) {
                    let mut msg = b"Expression about '".to_vec();
                    msg.extend(expr.string_with_ctx(ctx.eval_ctx(), RedactLogDisable));
                    msg.extend(
                        format!(
                            "' can not be pushed to TiFlash because it contains unsupported calculation of type '{}'.",
                            type_str(ft.get_type())
                        )
                        .bytes(),
                    );
                    ctx.append_warning(Error::plain(msg));
                    return false;
                }
            }
            mysql::TypeNewDecimal if !ft.is_decimal_valid() => {
                let mut msg = b"Expression about '".to_vec();
                msg.extend(expr.string_with_ctx(ctx.eval_ctx(), RedactLogDisable));
                msg.extend(
                    format!(
                        "' can not be pushed to TiFlash because it contains invalid decimal('{}','{}').",
                        ft.get_flen(),
                        ft.get_decimal()
                    )
                    .bytes(),
                );
                ctx.append_warning(Error::plain(msg));
                return false;
            }
            _ => {}
        }
    }
    match expr {
        Expr::CorrelatedColumn(x) => {
            pc.con_or_cor_col_to_pb_expr(expr).is_some()
                && pc.column_to_pb_expr(&x.column, true).is_some()
        }
        Expr::Constant(_) => pc.con_or_cor_col_to_pb_expr(expr).is_some(),
        Expr::Column(x) => pc.column_to_pb_expr(x, true).is_some(),
        Expr::ScalarFunction(x) => can_scalar_func_push_down(ctx, x, store_type),
    }
}

// go: scalarExprSupportedByTiDB L1
#[doc(hidden)] // unexported in Go; public for the vector replay
pub fn scalar_expr_supported_by_tidb(ctx: &dyn EvalContext, function: &ScalarFunction) -> bool {
    // TiDB can support all functions, but TiPB may not include some functions.
    scalar_expr_supported_by_tikv(ctx, function) || scalar_expr_supported_by_flash(ctx, function)
}

/// supported functions tracked by https://github.com/tikv/tikv/issues/5751
// go: scalarExprSupportedByTiKV L1
#[doc(hidden)] // unexported in Go; public for the vector replay
pub fn scalar_expr_supported_by_tikv(ctx: &dyn EvalContext, sf: &ScalarFunction) -> bool {
    match sf.func_name.l.as_str() {
        // op functions.
        ast::LogicAnd | ast::LogicOr | ast::LogicXor | ast::UnaryNot | ast::And | ast::Or
        | ast::Xor | ast::BitNeg | ast::LeftShift | ast::RightShift | ast::UnaryMinus
        // compare functions.
        | ast::LT | ast::LE | ast::EQ | ast::NE | ast::GE | ast::GT | ast::NullEQ | ast::In
        | ast::IsNull | ast::Like | ast::IsTruthWithoutNull | ast::IsTruthWithNull
        | ast::IsFalsity
        // ast.Greatest, ast.Least, ast.Interval

        // arithmetical functions.
        | ast::PI /* ast.Truncate */
        | ast::Plus | ast::Minus | ast::Mul | ast::Div | ast::Abs | ast::Mod | ast::IntDiv
        // math functions.
        | ast::Ceil | ast::Ceiling | ast::Floor | ast::Sqrt | ast::Sign | ast::Ln | ast::Log
        | ast::Log2 | ast::Log10 | ast::Exp | ast::Pow | ast::Power
        // Rust use the llvm math functions, which have different precision with Golang/MySQL(cmath)
        // open the following switchers if we implement them in coprocessor via `cmath`
        | ast::Sin | ast::Asin | ast::Cos | ast::Acos /* ast.Tan */ | ast::Atan | ast::Atan2
        | ast::Cot | ast::Radians | ast::Degrees | ast::CRC32
        // control flow functions.
        | ast::Case | ast::If | ast::Ifnull | ast::Coalesce
        // string functions.
        // ast.Bin, ast.Unhex, ast.Locate, ast.Ord, ast.Lpad, ast.Rpad,
        // ast.Trim, ast.FromBase64, ast.ToBase64, ast.InsertFunc,
        // ast.MakeSet, ast.SubstringIndex, ast.Instr, ast.Quote, ast.Oct,
        // ast.FindInSet, ast.Repeat,
        | ast::Upper | ast::Lower | ast::Length | ast::BitLength | ast::Concat | ast::ConcatWS
        | ast::Replace | ast::ASCII | ast::Hex | ast::Reverse | ast::LTrim | ast::RTrim
        | ast::Strcmp | ast::Space | ast::Elt | ast::Field | InternalFuncFromBinary
        | InternalFuncToBinary | ast::Mid | ast::Substring | ast::Substr | ast::CharLength
        | ast::Right /* ast.Left */
        // json functions.
        | ast::JSONType | ast::JSONExtract | ast::JSONObject | ast::JSONArray | ast::JSONMerge
        | ast::JSONSet | ast::JSONInsert | ast::JSONReplace | ast::JSONRemove | ast::JSONLength
        | ast::JSONMergePatch | ast::JSONUnquote | ast::JSONContains | ast::JSONValid
        | ast::JSONMemberOf | ast::JSONArrayAppend
        // vector functions.
        | ast::VecDims | ast::VecL1Distance | ast::VecL2Distance | ast::VecNegativeInnerProduct
        | ast::VecCosineDistance | ast::VecL2Norm | ast::VecAsText
        // date functions.
        | ast::Date | ast::Week /* ast.YearWeek, ast.ToSeconds */ | ast::DateDiff
        /* ast.TimeDiff, ast.AddTime,  ast.SubTime, */
        | ast::MonthName | ast::MakeDate | ast::TimeToSec | ast::MakeTime | ast::DateFormat
        | ast::DateAdd | ast::AddDate | ast::DateSub | ast::SubDate | ast::Hour | ast::Minute
        | ast::Second | ast::MicroSecond | ast::Month
        /* ast.DayName */ | ast::DayOfMonth | ast::DayOfWeek | ast::DayOfYear
        /* ast.Weekday */ | ast::WeekOfYear | ast::Year
        | ast::FromDays /* ast.ToDays */
        | ast::PeriodAdd | ast::PeriodDiff /*ast.TimestampDiff, ast.DateAdd, ast.FromUnixTime,*/
        /* ast.LastDay */
        | ast::Sysdate
        // encryption functions.
        | ast::MD5 | ast::SHA1 | ast::UncompressedLength
        | ast::Cast
        // misc functions.
        // TODO(#26942): enable functions below after them are fully tested in TiKV.
        /*ast.InetNtoa, ast.InetAton, ast.Inet6Ntoa, ast.Inet6Aton, ast.IsIPv4, ast.IsIPv4Compat, ast.IsIPv4Mapped, ast.IsIPv6,*/
        | ast::UUID => return true,
        // Rust use the llvm math functions, which have different precision with Golang/MySQL(cmath)
        // open the following switchers if we implement them in coprocessor via `cmath`
        ast::Conv => {
            let arg0 = &sf.get_args()[0];
            // To be aligned with MySQL, tidb handles hybrid type argument and binary literal specially, tikv can't be consistent with tidb now.
            if let Expr::ScalarFunction(f) = &**arg0
                && f.func_name.l == ast::Cast
                && (f.get_args()[0].get_type(ctx).hybrid() || is_binary_literal(&f.get_args()[0]))
            {
                return false;
            }
            return true;
        }
        ast::Round => {
            if matches!(
                sf.function.pb_code(),
                tipb::RoundReal | tipb::RoundInt | tipb::RoundDec
            ) {
                // We don't push round with frac due to mysql's round with frac has its special behavior:
                // https://dev.mysql.com/doc/refman/5.7/en/mathematical-functions.html#function_round
                return true;
            }
        }
        ast::Rand => {
            if sf.function.pb_code() == tipb::RandWithSeedFirstGen {
                return true;
            }
        }
        ast::Regexp | ast::RegexpLike | ast::RegexpSubstr | ast::RegexpInStr
        | ast::RegexpReplace => {
            let (func_charset, func_collation) = sf.function.collation_info.charset_and_collation();
            if func_charset == CharsetBin && func_collation == CollationBin {
                return false;
            }
            return true;
        }
        _ => {}
    }
    false
}

// go: scalarExprSupportedByFlash L1
#[doc(hidden)] // unexported in Go; public for the vector replay
pub fn scalar_expr_supported_by_flash(ctx: &dyn EvalContext, function: &ScalarFunction) -> bool {
    let pb_code = function.function.pb_code();
    match function.func_name.l.as_str() {
        ast::Floor | ast::Ceil | ast::Ceiling => {
            return !matches!(pb_code, tipb::FloorIntToDec | tipb::CeilIntToDec);
        }
        ast::LogicOr
        | ast::LogicAnd
        | ast::UnaryNot
        | ast::BitNeg
        | ast::Xor
        | ast::And
        | ast::Or
        | ast::RightShift
        | ast::LeftShift
        | ast::GE
        | ast::LE
        | ast::EQ
        | ast::NE
        | ast::LT
        | ast::GT
        | ast::In
        | ast::IsNull
        | ast::Like
        | ast::Ilike
        | ast::Strcmp
        | ast::Plus
        | ast::Minus
        | ast::Div
        | ast::Mul
        | ast::Abs
        | ast::Mod
        | ast::If
        | ast::Ifnull
        | ast::Case
        | ast::Concat
        | ast::ConcatWS
        | ast::Date
        | ast::Year
        | ast::Month
        | ast::Day
        | ast::Quarter
        | ast::DayName
        | ast::MonthName
        | ast::DateDiff
        | ast::TimestampDiff
        | ast::DateFormat
        | ast::FromUnixTime
        | ast::DayOfWeek
        | ast::DayOfMonth
        | ast::DayOfYear
        | ast::LastDay
        | ast::WeekOfYear
        | ast::ToSeconds
        | ast::FromDays
        | ast::ToDays
        | ast::Sqrt
        | ast::Log
        | ast::Log2
        | ast::Log10
        | ast::Ln
        | ast::Exp
        | ast::Pow
        | ast::Power
        | ast::Sign
        | ast::Radians
        | ast::Degrees
        | ast::Conv
        | ast::CRC32
        | ast::JSONLength
        | ast::JSONDepth
        | ast::JSONExtract
        | ast::JSONUnquote
        | ast::JSONArray
        | ast::JSONContainsPath
        | ast::JSONValid
        | ast::JSONKeys
        | ast::Repeat
        | ast::InetNtoa
        | ast::InetAton
        | ast::Inet6Ntoa
        | ast::Inet6Aton
        | ast::Coalesce
        | ast::ASCII
        | ast::Length
        | ast::Trim
        | ast::Position
        | ast::Format
        | ast::Elt
        | ast::LTrim
        | ast::RTrim
        | ast::Lpad
        | ast::Rpad
        | ast::Hour
        | ast::Minute
        | ast::Second
        | ast::MicroSecond
        | ast::TimeToSec => {
            if matches!(
                pb_code,
                tipb::InDuration
                    | tipb::CoalesceDuration
                    | tipb::IfNullDuration
                    | tipb::IfDuration
                    | tipb::CaseWhenDuration
                    | tipb::LTJson
                    | tipb::LEJson
                    | tipb::GTJson
                    | tipb::GEJson
                    | tipb::EQJson
                    | tipb::NEJson
                    | tipb::JsonIsNull
                    | tipb::InJson
            ) {
                return false;
            }
            return true;
        }
        ast::Regexp
        | ast::RegexpLike
        | ast::RegexpInStr
        | ast::RegexpSubstr
        | ast::RegexpReplace => {
            let (func_charset, func_collation) =
                function.function.collation_info.charset_and_collation();
            if func_charset == CharsetBin && func_collation == CollationBin {
                return false;
            }
            return true;
        }
        ast::Substr
        | ast::Substring
        | ast::Left
        | ast::Right
        | ast::CharLength
        | ast::SubstringIndex
        | ast::Reverse => {
            if matches!(
                pb_code,
                tipb::LeftUTF8
                    | tipb::RightUTF8
                    | tipb::CharLengthUTF8
                    | tipb::Substring2ArgsUTF8
                    | tipb::Substring3ArgsUTF8
                    | tipb::SubstringIndex
                    | tipb::ReverseUTF8
                    | tipb::Reverse
            ) {
                return true;
            }
        }
        ast::Cast => {
            let source_type = function.get_args()[0].get_type(ctx);
            let ret_type = function.ret_type();
            match pb_code {
                tipb::CastDecimalAsInt
                | tipb::CastIntAsInt
                | tipb::CastRealAsInt
                | tipb::CastTimeAsInt
                | tipb::CastStringAsInt /*, tipb::ScalarFuncSig_CastDurationAsInt, tipb::ScalarFuncSig_CastJsonAsInt*/ => {
                    // TiFlash cast only support cast to Int64 or the source type is the same as the target type
                    return (source_type.get_type() == ret_type.get_type()
                        && mysql::has_unsigned_flag(source_type.get_flag())
                            == mysql::has_unsigned_flag(ret_type.get_flag()))
                        || ret_type.get_type() == mysql::TypeLonglong;
                }
                tipb::CastIntAsReal
                | tipb::CastRealAsReal
                | tipb::CastStringAsReal
                | tipb::CastTimeAsReal
                | tipb::CastDecimalAsReal /*
                  tipb::ScalarFuncSig_CastDurationAsReal, tipb::ScalarFuncSig_CastJsonAsReal*/ => {
                    // TiFlash cast only support cast to Float64 or the source type is the same as the target type
                    return source_type.get_type() == ret_type.get_type()
                        || ret_type.get_type() == mysql::TypeDouble;
                }
                tipb::CastDecimalAsDecimal
                | tipb::CastIntAsDecimal
                | tipb::CastRealAsDecimal
                | tipb::CastTimeAsDecimal
                | tipb::CastStringAsDecimal /*, tipb::ScalarFuncSig_CastDurationAsDecimal, tipb::ScalarFuncSig_CastJsonAsDecimal*/ => {
                    return function.ret_type().is_decimal_valid();
                }
                tipb::CastDecimalAsString
                | tipb::CastIntAsString
                | tipb::CastRealAsString
                | tipb::CastTimeAsString
                | tipb::CastStringAsString
                | tipb::CastJsonAsString /*, tipb::ScalarFuncSig_CastDurationAsString*/ => {
                    return true;
                }
                tipb::CastDecimalAsTime
                | tipb::CastIntAsTime
                | tipb::CastRealAsTime
                | tipb::CastTimeAsTime
                | tipb::CastStringAsTime /*, tipb::ScalarFuncSig_CastDurationAsTime, tipb::ScalarFuncSig_CastJsonAsTime*/ => {
                    // ban the function of casting year type as time type pushing down to tiflash because of https://github.com/pingcap/tidb/issues/26215
                    return function.get_args()[0].get_type(ctx).get_type() != mysql::TypeYear;
                }
                tipb::CastTimeAsDuration => {
                    return ret_type.get_type() == mysql::TypeDuration;
                }
                tipb::CastVectorFloat32AsString | tipb::CastVectorFloat32AsVectorFloat32 => {
                    return true;
                }
                tipb::CastIntAsJson
                | tipb::CastRealAsJson
                | tipb::CastDecimalAsJson
                | tipb::CastStringAsJson
                | tipb::CastTimeAsJson
                | tipb::CastDurationAsJson
                | tipb::CastJsonAsJson => {
                    return true;
                }
                _ => {}
            }
        }
        ast::DateAdd | ast::AddDate => {
            if matches!(
                pb_code,
                tipb::AddDateDatetimeInt | tipb::AddDateStringInt | tipb::AddDateStringReal
            ) {
                return true;
            }
        }
        ast::DateSub | ast::SubDate => {
            if matches!(
                pb_code,
                tipb::SubDateDatetimeInt | tipb::SubDateStringInt | tipb::SubDateStringReal
            ) {
                return true;
            }
        }
        ast::UnixTimestamp => {
            if matches!(pb_code, tipb::UnixTimestampInt | tipb::UnixTimestampDec) {
                return true;
            }
        }
        ast::Round => {
            if matches!(
                pb_code,
                tipb::RoundInt
                    | tipb::RoundReal
                    | tipb::RoundDec
                    | tipb::RoundWithFracInt
                    | tipb::RoundWithFracReal
                    | tipb::RoundWithFracDec
            ) {
                return true;
            }
        }
        ast::Extract => {
            if matches!(pb_code, tipb::ExtractDatetime | tipb::ExtractDuration) {
                return true;
            }
        }
        ast::Replace => {
            if pb_code == tipb::Replace {
                return true;
            }
        }
        ast::StrToDate => {
            return matches!(pb_code, tipb::StrToDateDate | tipb::StrToDateDatetime);
        }
        ast::Upper | ast::Ucase | ast::Lower | ast::Lcase | ast::Space => return true,
        ast::Sysdate => return true,
        ast::Least | ast::Greatest => {
            if matches!(
                pb_code,
                tipb::GreatestInt
                    | tipb::GreatestReal
                    | tipb::LeastInt
                    | tipb::LeastReal
                    | tipb::LeastString
                    | tipb::GreatestString
            ) {
                return true;
            }
        }
        ast::IsTruthWithNull | ast::IsTruthWithoutNull | ast::IsFalsity => return true,
        ast::Hex | ast::Unhex | ast::Bin => return true,
        ast::GetFormat => return true,
        ast::IsIPv4 | ast::IsIPv6 => return true,
        ast::VecDims
        | ast::VecL1Distance
        | ast::VecL2Distance
        | ast::VecNegativeInnerProduct
        | ast::VecCosineDistance
        | ast::VecL2Norm
        | ast::VecAsText => return true,
        // grouping function for grouping sets identification.
        ast::Grouping => return true,
        _ => {}
    }
    false
}

// go: canEnumPushdownPreliminarily L1
#[doc(hidden)] // unexported in Go; public for the vector replay
pub fn can_enum_pushdown_preliminarily(scalar_func: &ScalarFunction) -> bool {
    match scalar_func.func_name.l.as_str() {
        ast::Cast => {
            let et = scalar_func.ret_type().eval_type();
            et == ETInt || et == ETReal || et == ETDecimal
        }
        _ => false,
    }
}

/// IsPushDownEnabled returns true if the input expr is not in the expr_pushdown_blacklist
// go: IsPushDownEnabled L1 [M-SNAP]
pub fn is_push_down_enabled(name: &str, store_type: StoreType) -> bool {
    if let Some(&value) = DefaultExprPushDownBlacklist.load().get(name) {
        let mask = store_type_mask(store_type);
        return value & mask != mask;
    }

    if store_type != TiFlash && name == ast::AggFuncApproxCountDistinct {
        // Can not push down approx_count_distinct to other store except tiflash by now.
        return false;
    }

    true
}

/// PushDownContext is the context used for push down expressions
#[derive(Clone)]
pub struct PushDownContext<'a> {
    eval_ctx: EvalContextRef,
    client: Arc<dyn Client>,
    warn_handler: Option<WarnAppenderRef>,
    group_concat_max_len: u64,
    /// The values of correlated columns the conversion reads (N4 §3.4): Go reads
    /// `CorrelatedColumn.Data`, the zero Datum while planning ([`empty_eval_state`]).
    st: &'a EvalState,
}

/// NewPushDownContext returns a new PushDownContext
// go: NewPushDownContext L1
pub fn new_push_down_context(
    eval_ctx: EvalContextRef,
    client: Arc<dyn Client>,
    in_explain_stmt: bool,
    warn_handler: Option<WarnAppenderRef>,
    extra_warn_handler: Option<WarnAppenderRef>,
    group_concat_max_len: u64,
) -> PushDownContext<'static> {
    let mut new_warn_handler = None;
    if let (Some(warn_handler), Some(extra_warn_handler)) = (warn_handler, extra_warn_handler) {
        if in_explain_stmt {
            new_warn_handler = Some(warn_handler);
        } else {
            new_warn_handler = Some(extra_warn_handler);
        }
    }

    PushDownContext {
        eval_ctx,
        client,
        warn_handler: new_warn_handler,
        group_concat_max_len,
        st: empty_eval_state(),
    }
}

/// NewPushDownContextFromSessionVars builds a new PushDownContext from session vars.
// go: NewPushDownContextFromSessionVars L1
pub fn new_push_down_context_from_session_vars(
    eval_ctx: EvalContextRef,
    sess_vars: &SessionVars,
    client: Arc<dyn Client>,
) -> PushDownContext<'static> {
    let sc = sess_vars.stmt_ctx();
    let (warn, extra): (WarnAppenderRef, WarnAppenderRef) =
        (sc.warn_handler(), sc.extra_warn_handler.clone());
    new_push_down_context(
        eval_ctx,
        client,
        sc.in_explain_stmt,
        Some(warn),
        Some(extra),
        sess_vars.group_concat_max_len,
    )
}

impl<'a> PushDownContext<'a> {
    /// The context reading correlated values from `st` (an executor deciding with the outer
    /// row's values; Go reads them through `CorrelatedColumn.Data`).
    // glue: the EvalState of the conversion (N4 §3.4)
    pub fn with_eval_state<'b>(self, st: &'b EvalState) -> PushDownContext<'b> {
        PushDownContext {
            eval_ctx: self.eval_ctx,
            client: self.client,
            warn_handler: self.warn_handler,
            group_concat_max_len: self.group_concat_max_len,
            st,
        }
    }

    /// EvalCtx returns the eval context
    // go: PushDownContext.EvalCtx L1
    pub fn eval_ctx(&self) -> &dyn EvalContext {
        &*self.eval_ctx
    }

    /// PbConverter returns a new PbConverter
    // go: PushDownContext.PbConverter L1
    pub fn pb_converter(&self) -> PbConverter<'_> {
        new_pb_converter(&*self.client, &*self.eval_ctx, self.st)
    }

    /// Client returns the kv client
    // go: PushDownContext.Client L1
    pub fn client(&self) -> &Arc<dyn Client> {
        &self.client
    }

    /// GetGroupConcatMaxLen returns the max length of group_concat
    // go: PushDownContext.GetGroupConcatMaxLen L1
    pub fn get_group_concat_max_len(&self) -> u64 {
        self.group_concat_max_len
    }

    /// AppendWarning appends a warning to be handled by the internal handler
    // go: PushDownContext.AppendWarning L1
    pub fn append_warning(&self, err: Error) {
        if let Some(h) = &self.warn_handler {
            h.append_warning(err);
        }
    }
}

/// PushDownExprsWithExtraInfo split the input exprs into pushed and remained, pushed include
/// all the exprs that can be pushed down
// go: PushDownExprsWithExtraInfo L1
pub fn push_down_exprs_with_extra_info(
    ctx: &PushDownContext<'_>,
    exprs: &[ExprRef],
    store_type: StoreType,
    can_enum_push: bool,
) -> (Vec<ExprRef>, Vec<ExprRef>) {
    let (mut pushed, mut remained) = (Vec::new(), Vec::new());
    for expr in exprs {
        if can_expr_push_down(ctx, expr, store_type, can_enum_push) {
            pushed.push(Arc::clone(expr));
        } else {
            remained.push(Arc::clone(expr));
        }
    }
    (pushed, remained)
}

/// PushDownExprs split the input exprs into pushed and remained, pushed include all the exprs
/// that can be pushed down
// go: PushDownExprs L1
pub fn push_down_exprs(
    ctx: &PushDownContext<'_>,
    exprs: &[ExprRef],
    store_type: StoreType,
) -> (Vec<ExprRef>, Vec<ExprRef>) {
    push_down_exprs_with_extra_info(ctx, exprs, store_type, false)
}

/// CanExprsPushDownWithExtraInfo return true if all the expr in exprs can be pushed down
// go: CanExprsPushDownWithExtraInfo L1
pub fn can_exprs_push_down_with_extra_info(
    ctx: &PushDownContext<'_>,
    exprs: &[ExprRef],
    store_type: StoreType,
    can_enum_push: bool,
) -> bool {
    let (_, remained) = push_down_exprs_with_extra_info(ctx, exprs, store_type, can_enum_push);
    remained.is_empty()
}

/// CanExprsPushDown return true if all the expr in exprs can be pushed down
// go: CanExprsPushDown L1
pub fn can_exprs_push_down(
    ctx: &PushDownContext<'_>,
    exprs: &[ExprRef],
    store_type: StoreType,
) -> bool {
    can_exprs_push_down_with_extra_info(ctx, exprs, store_type, false)
}

/// Go: `1 << storeType` on a uint32 (0 for a shift of 32 or more).
// go: storeTypeMask L1 [M-INT]
fn store_type_mask(store_type: StoreType) -> u32 {
    if store_type == UnSpecified {
        return 1 << TiKV.0 | 1 << TiFlash.0 | 1 << TiDB.0;
    }
    1u32.checked_shl(store_type.0 as u32).unwrap_or(0)
}

/// Go's `init` stores an empty map in `DefaultExprPushDownBlacklist` and creates
/// `ExprPushDownBlackListReloadTimeStamp` (zero, a static here).
// go: init L1 [M-SNAP]
fn init() -> ArcSwap<HashMap<String, u32>> {
    ArcSwap::from_pointee(HashMap::new())
}
