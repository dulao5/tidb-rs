//! go-file: pkg/expression/function_traits.go
//!
//! The function sets of `function_traits.go` (Go `map[string]struct{}`, here sorted-free
//! slices searched linearly: they are short and only consulted while building plans) and
//! `IsDeferredFunctions`. The two `opcode.Op` sets for partition expressions are with the
//! partition pruning block (D09.12).

#![allow(non_upper_case_globals)]

use crate::ast_funcs as ast;
use crate::context::BuildContext;

/// Whether `name` is in one of the sets below.
// glue: map lookup `_, ok := set[name]`
#[inline]
pub fn in_set(set: &[&str], name: &str) -> bool {
    set.contains(&name)
}

/// UnCacheableFunctions stores functions which can not be cached to plan cache.
pub static UnCacheableFunctions: &[&str] = &[
    ast::Database,
    ast::CurrentUser,
    ast::CurrentRole,
    ast::CurrentResourceGroup,
    ast::User,
    ast::ConnectionID,
    ast::LastInsertId,
    ast::RowCount,
    ast::Version,
    ast::Like,
    ast::JSONObject,
    ast::JSONArray,
    ast::Coalesce,
    ast::Convert,
    ast::TimeLiteral,
    ast::DateLiteral,
    ast::TimestampLiteral,
    ast::AesEncrypt,
    ast::AesDecrypt,
];

/// unFoldableFunctions stores functions which can not be folded duration constant folding stage.
pub static unFoldableFunctions: &[&str] = &[
    ast::Sysdate,
    ast::FoundRows,
    ast::Rand,
    ast::UUID,
    ast::Sleep,
    ast::RowFunc,
    ast::Values,
    ast::SetVar,
    ast::GetVar,
    ast::GetParam,
    ast::Benchmark,
    ast::DayName,
    ast::NextVal,
    ast::LastVal,
    ast::SetVal,
    ast::AnyValue,
];

/// DisableFoldFunctions stores functions which prevent child scope functions from being constant folded.
pub static DisableFoldFunctions: &[&str] = &[ast::Benchmark];

/// TryFoldFunctions stores functions which try to fold constant in child scope functions if without errors/warnings, otherwise, the child functions do not fold constant.
pub static TryFoldFunctions: &[&str] = &[
    ast::If,
    ast::Ifnull,
    ast::Case,
    ast::LogicAnd,
    ast::LogicOr,
    ast::Coalesce,
    ast::Interval,
];

/// IllegalFunctions4GeneratedColumns stores functions that is illegal for generated columns.
pub static IllegalFunctions4GeneratedColumns: &[&str] = &[
    ast::ConnectionID,
    ast::LoadFile,
    ast::LastInsertId,
    ast::Rand,
    ast::UUID,
    ast::UUIDShort,
    ast::Curdate,
    ast::CurrentDate,
    ast::Curtime,
    ast::CurrentTime,
    ast::CurrentTimestamp,
    ast::LocalTime,
    ast::LocalTimestamp,
    ast::Now,
    ast::UnixTimestamp,
    ast::UTCDate,
    ast::UTCTime,
    ast::UTCTimestamp,
    ast::Benchmark,
    ast::CurrentUser,
    ast::Database,
    ast::FoundRows,
    ast::GetLock,
    ast::IsFreeLock,
    ast::IsUsedLock,
    ast::MasterPosWait,
    ast::NameConst,
    ast::ReleaseLock,
    ast::RowFunc,
    ast::RowCount,
    ast::Schema,
    ast::SessionUser,
    ast::Sleep,
    ast::Sysdate,
    ast::SystemUser,
    ast::User,
    ast::Values,
    ast::Encrypt,
    ast::Version,
    ast::JSONMerge,
    ast::SetVar,
    ast::GetVar,
    ast::ReleaseAllLocks,
];

/// IsDeferredFunctions checks whether the function is in DeferredFunctions.
/// DeferredFunctions stores functions which are foldable but should be deferred as well when
/// plan cache is enabled. Note that, these functions must be foldable at first place, i.e,
/// they are not in `unFoldableFunctions`.
// go: IsDeferredFunctions L1
pub fn is_deferred_functions(ctx: &dyn BuildContext, fn_name: &str) -> bool {
    let ok = in_set(deferredFunctions, fn_name);
    if ok {
        return ok;
    }
    if fn_name == ast::Sysdate && ctx.get_sysdate_is_now() {
        return true;
    }
    ok
}

/// deferredFunctions stores functions which are foldable but should be deferred as well when plan cache is enabled.
pub static deferredFunctions: &[&str] = &[
    ast::Now,
    ast::RandomBytes,
    ast::CurrentTimestamp,
    ast::UTCTime,
    ast::Curtime,
    ast::CurrentTime,
    ast::UTCTimestamp,
    ast::UnixTimestamp,
    ast::Curdate,
    ast::CurrentDate,
    ast::UTCDate,
];

/// AllowedPartitionFuncMap stores functions which can be used in the partition expression.
pub static AllowedPartitionFuncMap: &[&str] = &[
    ast::ToDays,
    ast::ToSeconds,
    ast::DayOfMonth,
    ast::Month,
    ast::DayOfYear,
    ast::Quarter,
    ast::YearWeek,
    ast::Year,
    ast::Weekday,
    ast::DayOfWeek,
    ast::Day,
    ast::Hour,
    ast::Minute,
    ast::Second,
    ast::TimeToSec,
    ast::MicroSecond,
    ast::UnixTimestamp,
    ast::FromDays,
    ast::Extract,
    ast::Abs,
    ast::Ceiling,
    ast::DateDiff,
    ast::Floor,
    ast::Mod,
];

/// inequalFunctions stores functions which cannot be propagated from column equal condition.
pub static inequalFunctions: &[&str] = &[ast::IsNull];

/// mutableEffectsFunctions stores functions which are mutable or have side effects, specifically, we cannot remove them from filter even if they have duplicates.
pub static mutableEffectsFunctions: &[&str] = &[
    ast::Now,
    ast::CurrentTimestamp,
    ast::UTCTime,
    ast::Curtime,
    ast::CurrentTime,
    ast::UTCTimestamp,
    ast::UnixTimestamp,
    ast::Sysdate,
    ast::Curdate,
    ast::CurrentDate,
    ast::UTCDate,
    ast::Rand,
    ast::RandomBytes,
    ast::UUID,
    ast::UUIDShort,
    ast::Sleep,
    ast::SetVar,
    ast::GetVar,
    ast::AnyValue,
];

/// noopFuncs: functions that only have noop implementations (empty in v8.5.8).
pub static noopFuncs: &[&str] = &[];

/// booleanFunctions stores boolean functions.
pub static booleanFunctions: &[&str] = &[
    ast::UnaryNot,
    ast::EQ,
    ast::NE,
    ast::NullEQ,
    ast::LT,
    ast::LE,
    ast::GT,
    ast::GE,
    ast::In,
    ast::LogicAnd,
    ast::LogicOr,
    ast::LogicXor,
    ast::IsTruthWithNull,
    ast::IsTruthWithoutNull,
    ast::IsFalsity,
    ast::IsNull,
    ast::Like,
    ast::Regexp,
    ast::IsIPv4,
    ast::IsIPv4Compat,
    ast::IsIPv4Mapped,
    ast::IsIPv6,
    ast::JSONSchemaValid,
    ast::JSONValid,
    ast::RegexpLike,
];
