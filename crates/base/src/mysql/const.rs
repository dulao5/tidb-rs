//! go-file: pkg/parser/mysql/const.go
//!
//! Constants and the maps of this file are in `const_gen.rs` (generated); this file has the
//! types, the variables and the functions.

// Go identifiers for constants and statics (mapping rule M-GONAME).
#![allow(non_upper_case_globals)]

use std::ops::{BitAnd, BitOr, Not};
use std::sync::LazyLock;

use parking_lot::RwLock;

use super::{CombinationSQLMode, ErrWrongValueForVar, Str2SQLMode, VersionSeparator};
use super::{DelayedPriority, HighPriority, LowPriority, NoPriority};
use super::{
    ModeANSIQuotes, ModeAllowInvalidDates, ModeErrorForDivisionByZero, ModeHighNotPrecedence,
    ModeIgnoreSpace, ModeNoAutoCreateUser, ModeNoBackslashEscapes, ModeNoUnsignedSubtraction,
    ModeNoZeroDate, ModeNoZeroInDate, ModeOnlyFullGroupBy, ModePadCharToFullLength,
    ModePipesAsConcat, ModeRealAsFloat, ModeStrictAllTables, ModeStrictTransTables, SQLError,
    ServerStatusCursorExists,
};
use super::{format_en_us, format_not_support, format_zh_cn, mysqlCompatibilityVersion, new_err};
use crate::errors::{self, Error};
use crate::go_args;

// go: newInvalidModeErr L1
fn new_invalid_mode_err(s: &str) -> SQLError {
    new_err(ErrWrongValueForVar, go_args!["sql_mode", s.to_owned()])
}

// Version information.

/// TiDBReleaseVersion is initialized by (git describe --tags) in Makefile.
/// (Go: a package variable set by `-ldflags`.)
pub static TiDBReleaseVersion: LazyLock<RwLock<String>> =
    LazyLock::new(|| RwLock::new("v8.4.0-this-is-a-placeholder".to_owned()));

/// ServerVersion is the version information of this tidb-server in MySQL's format.
/// (Go: a package variable; the server overwrites it from the config.)
pub static ServerVersion: LazyLock<RwLock<String>> = LazyLock::new(|| {
    RwLock::new(format!(
        "{}{}{}",
        mysqlCompatibilityVersion,
        VersionSeparator,
        TiDBReleaseVersion.read()
    ))
});

/// HasCursorExistsFlag return true if cursor exists indicated by server status.
// go: HasCursorExistsFlag L1
pub fn has_cursor_exists_flag(server_status: u16) -> bool {
    server_status & ServerStatusCursorExists > 0
}

/// SQLMode is the type for MySQL sql_mode.
/// See https://dev.mysql.com/doc/refman/5.7/en/sql-mode.html
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SQLMode(pub i64);

impl BitAnd for SQLMode {
    type Output = SQLMode;
    // glue: Go integer & on the named type SQLMode
    fn bitand(self, rhs: SQLMode) -> SQLMode {
        SQLMode(self.0 & rhs.0)
    }
}
impl BitOr for SQLMode {
    type Output = SQLMode;
    // glue: Go integer | on the named type SQLMode
    fn bitor(self, rhs: SQLMode) -> SQLMode {
        SQLMode(self.0 | rhs.0)
    }
}
impl Not for SQLMode {
    type Output = SQLMode;
    // glue: Go integer ^ (bitwise not) on the named type SQLMode
    fn not(self) -> SQLMode {
        SQLMode(!self.0)
    }
}

impl SQLMode {
    /// HasNoZeroDateMode detects if 'NO_ZERO_DATE' mode is set in SQLMode
    // go: SQLMode.HasNoZeroDateMode L1
    pub fn has_no_zero_date_mode(self) -> bool {
        self & ModeNoZeroDate == ModeNoZeroDate
    }

    /// HasNoZeroInDateMode detects if 'NO_ZERO_IN_DATE' mode is set in SQLMode
    // go: SQLMode.HasNoZeroInDateMode L1
    pub fn has_no_zero_in_date_mode(self) -> bool {
        self & ModeNoZeroInDate == ModeNoZeroInDate
    }

    /// HasErrorForDivisionByZeroMode detects if 'ERROR_FOR_DIVISION_BY_ZERO' mode is set in SQLMode
    // go: SQLMode.HasErrorForDivisionByZeroMode L1
    pub fn has_error_for_division_by_zero_mode(self) -> bool {
        self & ModeErrorForDivisionByZero == ModeErrorForDivisionByZero
    }

    /// HasOnlyFullGroupBy detects if 'ONLY_FULL_GROUP_BY' mode is set in SQLMode
    // go: SQLMode.HasOnlyFullGroupBy L1
    pub fn has_only_full_group_by(self) -> bool {
        self & ModeOnlyFullGroupBy == ModeOnlyFullGroupBy
    }

    /// HasStrictMode detects if 'STRICT_TRANS_TABLES' or 'STRICT_ALL_TABLES' mode is set in SQLMode
    // go: SQLMode.HasStrictMode L1
    pub fn has_strict_mode(self) -> bool {
        self & ModeStrictTransTables == ModeStrictTransTables
            || self & ModeStrictAllTables == ModeStrictAllTables
    }

    /// HasPipesAsConcatMode detects if 'PIPES_AS_CONCAT' mode is set in SQLMode
    // go: SQLMode.HasPipesAsConcatMode L1
    pub fn has_pipes_as_concat_mode(self) -> bool {
        self & ModePipesAsConcat == ModePipesAsConcat
    }

    /// HasNoUnsignedSubtractionMode detects if 'NO_UNSIGNED_SUBTRACTION' mode is set in SQLMode
    // go: SQLMode.HasNoUnsignedSubtractionMode L1
    pub fn has_no_unsigned_subtraction_mode(self) -> bool {
        self & ModeNoUnsignedSubtraction == ModeNoUnsignedSubtraction
    }

    /// HasHighNotPrecedenceMode detects if 'HIGH_NOT_PRECEDENCE' mode is set in SQLMode
    // go: SQLMode.HasHighNotPrecedenceMode L1
    pub fn has_high_not_precedence_mode(self) -> bool {
        self & ModeHighNotPrecedence == ModeHighNotPrecedence
    }

    /// HasANSIQuotesMode detects if 'ANSI_QUOTES' mode is set in SQLMode
    // go: SQLMode.HasANSIQuotesMode L1
    pub fn has_ansi_quotes_mode(self) -> bool {
        self & ModeANSIQuotes == ModeANSIQuotes
    }

    /// HasRealAsFloatMode detects if 'REAL_AS_FLOAT' mode is set in SQLMode
    // go: SQLMode.HasRealAsFloatMode L1
    pub fn has_real_as_float_mode(self) -> bool {
        self & ModeRealAsFloat == ModeRealAsFloat
    }

    /// HasPadCharToFullLengthMode detects if 'PAD_CHAR_TO_FULL_LENGTH' mode is set in SQLMode
    // go: SQLMode.HasPadCharToFullLengthMode L1
    pub fn has_pad_char_to_full_length_mode(self) -> bool {
        self & ModePadCharToFullLength == ModePadCharToFullLength
    }

    /// HasNoBackslashEscapesMode detects if 'NO_BACKSLASH_ESCAPES' mode is set in SQLMode
    // go: SQLMode.HasNoBackslashEscapesMode L1
    pub fn has_no_backslash_escapes_mode(self) -> bool {
        self & ModeNoBackslashEscapes == ModeNoBackslashEscapes
    }

    /// HasIgnoreSpaceMode detects if 'IGNORE_SPACE' mode is set in SQLMode
    // go: SQLMode.HasIgnoreSpaceMode L1
    pub fn has_ignore_space_mode(self) -> bool {
        self & ModeIgnoreSpace == ModeIgnoreSpace
    }

    /// HasNoAutoCreateUserMode detects if 'NO_AUTO_CREATE_USER' mode is set in SQLMode
    // go: SQLMode.HasNoAutoCreateUserMode L1
    pub fn has_no_auto_create_user_mode(self) -> bool {
        self & ModeNoAutoCreateUser == ModeNoAutoCreateUser
    }

    /// HasAllowInvalidDatesMode detects if 'ALLOW_INVALID_DATES' mode is set in SQLMode
    // go: SQLMode.HasAllowInvalidDatesMode L1
    pub fn has_allow_invalid_dates_mode(self) -> bool {
        self & ModeAllowInvalidDates == ModeAllowInvalidDates
    }
}

/// DelSQLMode delete sql mode from ori
// go: DelSQLMode L1
pub fn del_sql_mode(ori: SQLMode, del: SQLMode) -> SQLMode {
    ori & (!del)
}

/// SetSQLMode add sql mode to ori
// go: SetSQLMode L1
pub fn set_sql_mode(ori: SQLMode, add: SQLMode) -> SQLMode {
    ori | add
}

/// FormatSQLModeStr re-format 'SQL_MODE' variable.
// go: FormatSQLModeStr L1
pub fn format_sql_mode_str(s: &str) -> String {
    let s = crate::gostd::to_upper(s.trim_end_matches(' '));
    let parts = s.split(',');
    let mut non_empty_parts: Vec<String> = Vec::new();
    let mut exist_parts = std::collections::HashSet::new();
    for part in parts {
        if part.is_empty() {
            continue;
        }
        if let Some(mode_parts) = CombinationSQLMode.get(part) {
            for mode_part in mode_parts.iter() {
                if exist_parts.insert((*mode_part).to_owned()) {
                    non_empty_parts.push((*mode_part).to_owned());
                }
            }
        }
        if exist_parts.insert(part.to_owned()) {
            non_empty_parts.push(part.to_owned());
        }
    }
    non_empty_parts.join(",")
}

/// GetSQLMode gets the sql mode for string literal. SQL_mode is a list of different modes
/// separated by commas. The input string must be formatted by 'FormatSQLModeStr'
// go: GetSQLMode L1 [M-ERRVAL]
pub fn get_sql_mode(s: &str) -> (SQLMode, Result<(), SQLError>) {
    let strs: Vec<&str> = s.split(',').collect();
    let mut sql_mode = SQLMode(0);
    for s in &strs {
        let mode = Str2SQLMode.get(*s).copied();
        if mode.is_none() && !s.is_empty() {
            return (sql_mode, Err(new_invalid_mode_err(s)));
        }
        sql_mode = sql_mode | mode.unwrap_or_default();
    }
    (sql_mode, Ok(()))
}

/// FormatFunc is the locale format function signature.
pub type FormatFunc = fn(&[u8], &[u8]) -> Result<Vec<u8>, Error>;

/// GetLocaleFormatFunction get the format function for sepcific locale.
// go: GetLocaleFormatFunction L1
pub fn get_locale_format_function(loc: &str) -> FormatFunc {
    match locale2_format_function(loc) {
        Some(f) => f,
        None => format_not_support,
    }
}

/// locale2FormatFunction is the string represent of locale format function.
// glue: Go map literal locale2FormatFunction (two entries, function values)
fn locale2_format_function(loc: &str) -> Option<FormatFunc> {
    match loc {
        "en_US" => Some(format_en_us),
        "zh_CN" => Some(format_zh_cn),
        _ => None,
    }
}

/// PriorityEnum is defined for Priority const values.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PriorityEnum(pub isize);

/// Str2Priority is used to convert a string to a priority.
// go: Str2Priority L1
pub fn str2_priority(val: &str) -> PriorityEnum {
    let val = crate::gostd::to_upper(val);
    match val.as_str() {
        "NO_PRIORITY" => NoPriority,
        "HIGH_PRIORITY" => HighPriority,
        "LOW_PRIORITY" => LowPriority,
        "DELAYED" => DelayedPriority,
        _ => NoPriority,
    }
}

/// The part of `format.RestoreCtx` that `PriorityEnum.Restore` uses. `RestoreCtx` is in the
/// parser (crate `tidb-parser`, a higher layer): the interface is defined here and the
/// parser implements it (N0 §4 rule 2, interface inversion).
pub trait RestoreKeyWord {
    /// `RestoreCtx.WriteKeyWord`.
    // glue: method of the interface-inversion trait (format.RestoreCtx.WriteKeyWord)
    fn write_key_word(&mut self, key_word: &str);
}

impl PriorityEnum {
    /// Restore implements Node interface.
    // go: PriorityEnum.Restore L2 (format.RestoreCtx through the RestoreKeyWord trait, N0 §4 rule 2)
    pub fn restore(&self, ctx: &mut dyn RestoreKeyWord) -> Result<(), Error> {
        match *self {
            NoPriority => return Ok(()),
            LowPriority => ctx.write_key_word("LOW_PRIORITY"),
            HighPriority => ctx.write_key_word("HIGH_PRIORITY"),
            DelayedPriority => ctx.write_key_word("DELAYED"),
            _ => {
                return Err(errors::errorf(
                    "undefined PriorityEnum Type[%d]",
                    &[crate::Arg::from(self.0)],
                ));
            }
        }
        Ok(())
    }
}
