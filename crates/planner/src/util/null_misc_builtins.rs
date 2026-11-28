//! go-file: pkg/planner/util/null_misc_builtins.go
//!
//! The two classification tables of the null-reject proof (`util/null_misc.go`). Go keeps
//! them as package-level maps; here the null-preserving set is a slice looked up
//! through a hash set built once (`LazyLock`), and the IS TRUE / IS FALSE tests a `match`.

use std::collections::HashSet;
use std::sync::LazyLock;

use tidb_parser::ast::funcs as ast_funcs;

/// nullRejectTestMode describes how IS TRUE / IS FALSE style builtins handle a
/// NULL child during null-reject proof.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NullRejectTestMode {
    /// f(NULL) = FALSE; nonTrue but not mustNull
    ReturnsFalse,
    /// f(NULL) = NULL; both nonTrue and mustNull
    KeepsNull,
}

/// nullRejectNullPreservingFunctions lists builtins that return NULL when any
/// argument is NULL. See the file-level comment in null_misc.go for how this
/// drives the proof.
const NULL_PRESERVING_FUNCTIONS: &[&str] = &[
    // Cast and unary operators.
    ast_funcs::Cast,
    ast_funcs::UnaryNot,
    ast_funcs::UnaryMinus,
    ast_funcs::BitNeg,
    // Comparison and boolean operators.
    ast_funcs::Greatest,
    ast_funcs::Least,
    ast_funcs::BitCount,
    ast_funcs::GE,
    ast_funcs::LE,
    ast_funcs::EQ,
    ast_funcs::NE,
    ast_funcs::LT,
    ast_funcs::GT,
    ast_funcs::LogicXor,
    // Arithmetic and bitwise operators.
    ast_funcs::Plus,
    ast_funcs::Minus,
    ast_funcs::Mod,
    ast_funcs::Div,
    ast_funcs::Mul,
    ast_funcs::IntDiv,
    ast_funcs::And,
    ast_funcs::LeftShift,
    ast_funcs::RightShift,
    ast_funcs::Or,
    ast_funcs::Xor,
    // Pattern matching.
    ast_funcs::Like,
    ast_funcs::Ilike,
    ast_funcs::Regexp,
    ast_funcs::RegexpLike,
    ast_funcs::RegexpSubstr,
    ast_funcs::RegexpInStr,
    ast_funcs::RegexpReplace,
    ast_funcs::Strcmp,
    // Mathematical functions.
    ast_funcs::Abs,
    ast_funcs::Acos,
    ast_funcs::Asin,
    ast_funcs::Atan,
    ast_funcs::Atan2,
    ast_funcs::Ceil,
    ast_funcs::Ceiling,
    ast_funcs::Conv,
    ast_funcs::Cos,
    ast_funcs::Cot,
    ast_funcs::CRC32,
    ast_funcs::Degrees,
    ast_funcs::Exp,
    ast_funcs::Floor,
    ast_funcs::Ln,
    ast_funcs::Log,
    ast_funcs::Log2,
    ast_funcs::Log10,
    ast_funcs::Pow,
    ast_funcs::Power,
    ast_funcs::Radians,
    ast_funcs::Round,
    ast_funcs::Sign,
    ast_funcs::Sin,
    ast_funcs::Sqrt,
    ast_funcs::Tan,
    // String functions.
    ast_funcs::ASCII,
    ast_funcs::Bin,
    ast_funcs::BitLength,
    ast_funcs::CharLength,
    ast_funcs::CharacterLength,
    ast_funcs::Concat,
    ast_funcs::FindInSet,
    ast_funcs::FromBase64,
    ast_funcs::Hex,
    ast_funcs::InsertFunc,
    ast_funcs::Instr,
    ast_funcs::Lcase,
    ast_funcs::Left,
    ast_funcs::Length,
    ast_funcs::Locate,
    ast_funcs::Lower,
    ast_funcs::Lpad,
    ast_funcs::LTrim,
    ast_funcs::Mid,
    ast_funcs::Oct,
    ast_funcs::OctetLength,
    ast_funcs::Ord,
    ast_funcs::Position,
    ast_funcs::Repeat,
    ast_funcs::Replace,
    ast_funcs::Reverse,
    ast_funcs::Right,
    ast_funcs::Rpad,
    ast_funcs::RTrim,
    ast_funcs::Space,
    ast_funcs::Substr,
    ast_funcs::Substring,
    ast_funcs::SubstringIndex,
    ast_funcs::ToBase64,
    ast_funcs::Translate,
    ast_funcs::Trim,
    ast_funcs::Ucase,
    ast_funcs::Unhex,
    ast_funcs::Upper,
    ast_funcs::WeightString,
    // Date and time functions.
    ast_funcs::AddDate,
    ast_funcs::DateAdd,
    ast_funcs::SubDate,
    ast_funcs::DateSub,
    ast_funcs::AddTime,
    ast_funcs::ConvertTz,
    ast_funcs::Date,
    ast_funcs::DateFormat,
    ast_funcs::DateDiff,
    ast_funcs::Day,
    ast_funcs::DayName,
    ast_funcs::DayOfMonth,
    ast_funcs::DayOfWeek,
    ast_funcs::DayOfYear,
    ast_funcs::Extract,
    ast_funcs::FromDays,
    ast_funcs::FromUnixTime,
    ast_funcs::Hour,
    ast_funcs::LastDay,
    ast_funcs::MakeDate,
    ast_funcs::MakeTime,
    ast_funcs::MicroSecond,
    ast_funcs::Minute,
    ast_funcs::Month,
    ast_funcs::MonthName,
    ast_funcs::PeriodAdd,
    ast_funcs::PeriodDiff,
    ast_funcs::Quarter,
    ast_funcs::SecToTime,
    ast_funcs::Second,
    ast_funcs::StrToDate,
    ast_funcs::SubTime,
    ast_funcs::Time,
    ast_funcs::TimeDiff,
    ast_funcs::TimeFormat,
    ast_funcs::TimeToSec,
    ast_funcs::Timestamp,
    ast_funcs::TimestampAdd,
    ast_funcs::TimestampDiff,
    ast_funcs::ToDays,
    ast_funcs::ToSeconds,
    ast_funcs::UnixTimestamp,
    ast_funcs::Weekday,
    ast_funcs::WeekOfYear,
    ast_funcs::Year,
    // Encryption, hashing, and compression.
    ast_funcs::Compress,
    ast_funcs::MD5,
    ast_funcs::SHA1,
    ast_funcs::SHA,
    ast_funcs::SHA2,
    ast_funcs::SM3,
    ast_funcs::Uncompress,
    ast_funcs::UncompressedLength,
    // JSON functions.
    ast_funcs::JSONType,
    ast_funcs::JSONExtract,
    ast_funcs::JSONUnquote,
    ast_funcs::JSONRemove,
    ast_funcs::JSONMerge,
    ast_funcs::JSONMergePreserve,
    ast_funcs::JSONContains,
    ast_funcs::JSONContainsPath,
    ast_funcs::JSONOverlaps,
    ast_funcs::JSONMemberOf,
    ast_funcs::JSONValid,
    ast_funcs::JSONPretty,
    ast_funcs::JSONQuote,
    ast_funcs::JSONStorageFree,
    ast_funcs::JSONStorageSize,
    ast_funcs::JSONDepth,
    ast_funcs::JSONKeys,
    ast_funcs::JSONLength,
    // Network address functions.
    ast_funcs::InetAton,
    ast_funcs::InetNtoa,
    ast_funcs::Inet6Aton,
    ast_funcs::Inet6Ntoa,
];

/// The set of [`NULL_PRESERVING_FUNCTIONS`] (Go's `nullRejectNullPreservingFunctions` map).
pub(crate) static NULL_REJECT_NULL_PRESERVING_FUNCTIONS: LazyLock<HashSet<&'static str>> =
    LazyLock::new(|| NULL_PRESERVING_FUNCTIONS.iter().copied().collect());

/// nullRejectRejectNullTests lists IS-TRUE / IS-FALSE style builtins that
/// convert a NULL input into a definite boolean. The mode records whether the
/// output is FALSE (ReturnsFalse) or stays NULL (KeepsNull).
///
/// Go's `nullRejectRejectNullTests[name]` lookup: `None` is a missing key.
// glue: the map nullRejectRejectNullTests as a match (a Go package variable, not a function)
pub(crate) fn null_reject_reject_null_tests(name: &str) -> Option<NullRejectTestMode> {
    match name {
        ast_funcs::IsTruthWithoutNull => Some(NullRejectTestMode::ReturnsFalse),
        ast_funcs::IsTruthWithNull => Some(NullRejectTestMode::KeepsNull),
        ast_funcs::IsFalsity => Some(NullRejectTestMode::ReturnsFalse),
        _ => None,
    }
}
