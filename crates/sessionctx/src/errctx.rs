//! go-file: pkg/errctx/context.go
//!
//! `errctx`: how a statement handles an error of each group (return it, turn it into a
//! warning, or ignore it).
//!
//! Mapping:
//! - `Context` is a value type in Go (a `LevelMap` array and a `WarnAppender` interface
//!   value): here a `Clone` struct holding an `Option<WarnAppenderRef>` (nil is possible
//!   after `WithStrictErrGroupLevel` of a zero `Context`).
//! - Go `error` is [`Error`] (M-ERR): `HandleError(nil)` returns nil in Go; the Rust
//!   methods take an error and return `Result<(), Error>`.
//! - `errors.ErrorGroup` (an error wrapping several errors) has no counterpart in M-ERR's
//!   single error type, so the group branch of `HandleError` is not ported (L2). Go reaches it
//!   on the INSERT write path (rowcodec joins the column errors with multierr →
//!   `WriteMemBufferEncoded` → `HandleError` per error level); the read-only tidb-rs does
//!   not, and porting table writes needs an error-group form of M-ERR (D00.1 spec §4).

// Constants keep their Go names (mapping rule M-GONAME).
#![allow(non_upper_case_globals)]

use std::sync::LazyLock;

use tidb_base::Error;
use tidb_base::contextutil::{WarnAppenderRef, ignore_warn_appender};
use tidb_base::errno;

/// Level defines the behavior for each error
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Level(pub u8);

/// LevelError means the error will be returned
pub const LevelError: Level = Level(0);
/// LevelWarn means it will be regarded as a warning
pub const LevelWarn: Level = Level(1);
/// LevelIgnore means the error will be ignored
pub const LevelIgnore: Level = Level(2);

/// ErrGroup groups the error according to the behavior of handling errors
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct ErrGroup(pub i64);

/// ErrGroupTruncate is the group of truncated errors
pub const ErrGroupTruncate: ErrGroup = ErrGroup(0);
/// ErrGroupDupKey is the group of duplicate key errors
pub const ErrGroupDupKey: ErrGroup = ErrGroup(1);
/// ErrGroupBadNull is the group of bad null errors
pub const ErrGroupBadNull: ErrGroup = ErrGroup(2);
/// ErrGroupNoDefault is the group of no default value errors
pub const ErrGroupNoDefault: ErrGroup = ErrGroup(3);
/// ErrGroupDividedByZero is the group of divided by zero errors
pub const ErrGroupDividedByZero: ErrGroup = ErrGroup(4);
/// ErrGroupAutoIncReadFailed is the group of auto increment read failed errors
pub const ErrGroupAutoIncReadFailed: ErrGroup = ErrGroup(5);
/// ErrGroupNoMatchedPartition is the group of no partition is matched errors.
pub const ErrGroupNoMatchedPartition: ErrGroup = ErrGroup(6);
/// errGroupCount is the count of all `ErrGroup`. Please leave it at the end of the list.
const errGroupCount: usize = 7;

/// LevelMap indicates the map from `ErrGroup` to `Level`
pub type LevelMap = [Level; errGroupCount];

/// Context defines how to handle an error
#[derive(Clone, Default)]
pub struct Context {
    level_map: LevelMap,
    warn_handler: Option<WarnAppenderRef>,
}

impl std::fmt::Debug for Context {
    // glue: Debug for a struct holding a trait object
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Context")
            .field("level_map", &self.level_map)
            .field("warn_handler", &self.warn_handler.is_some())
            .finish()
    }
}

/// The level map entry of `eg`: Go indexes the array and panics out of range.
// glue: Go array index levelMap[eg]
fn index(eg: ErrGroup) -> usize {
    if eg.0 < 0 || eg.0 as usize >= errGroupCount {
        panic!(
            "runtime error: index out of range [{}] with length {errGroupCount}",
            eg.0
        );
    }
    eg.0 as usize
}

impl Context {
    /// LevelMap returns the `levelMap` of the context.
    // go: Context.LevelMap L1
    pub fn level_map(&self) -> LevelMap {
        self.level_map
    }

    /// LevelForGroup returns the level for a specified group.
    // go: Context.LevelForGroup L1 [M-PANIC]
    pub fn level_for_group(&self, err_group: ErrGroup) -> Level {
        self.level_map[index(err_group)]
    }

    /// WithStrictErrGroupLevel makes the context to return the error directly for any kinds
    /// of errors.
    // go: Context.WithStrictErrGroupLevel L1
    pub fn with_strict_err_group_level(&self) -> Context {
        Context {
            level_map: LevelMap::default(),
            warn_handler: self.warn_handler.clone(),
        }
    }

    /// WithErrGroupLevel sets a `Level` for an `ErrGroup`
    // go: Context.WithErrGroupLevel L1 [M-PANIC]
    pub fn with_err_group_level(&self, eg: ErrGroup, l: Level) -> Context {
        let mut new_ctx = Context {
            level_map: self.level_map,
            warn_handler: self.warn_handler.clone(),
        };
        new_ctx.level_map[index(eg)] = l;
        new_ctx
    }

    /// WithErrGroupLevels sets `levelMap` for an `ErrGroup`
    // go: Context.WithErrGroupLevels L1
    pub fn with_err_group_levels(&self, levels: LevelMap) -> Context {
        Context {
            level_map: levels,
            warn_handler: self.warn_handler.clone(),
        }
    }

    /// AppendWarning appends the error to warning. If the inner `warnHandler` is nil, do
    /// nothing.
    // go: Context.AppendWarning L1
    pub fn append_warning(&self, err: Error) {
        debug_assert!(
            self.warn_handler.is_some(),
            "intest.Assert(warnHandler != nil)"
        );
        if let Some(w) = &self.warn_handler {
            // warnHandler should always not be nil, check fn != nil here to just make code safe.
            w.append_warning(err);
        }
    }

    /// AppendNote appends the error to warning with level 'Note'. If the inner
    /// `warnHandler` is nil, do nothing.
    // go: Context.AppendNote L1
    pub fn append_note(&self, err: Error) {
        debug_assert!(
            self.warn_handler.is_some(),
            "intest.Assert(warnHandler != nil)"
        );
        if let Some(w) = &self.warn_handler {
            w.append_note(err);
        }
    }

    /// HandleError handles the error according to the contextutil. See the comment of
    /// `HandleErrorWithAlias` for detailed logic.
    ///
    /// Go also accepts an `errors.ErrorGroup` and handles each error in order; M-ERR has no
    /// error groups.
    // go: Context.HandleError L2 (errors.ErrorGroup is not representable, M-ERR)
    pub fn handle_error(&self, err: Error) -> Result<(), Error> {
        let warn = err.clone();
        self.handle_error_with_alias(err.clone(), err, warn)
    }

    /// HandleErrorWithAlias handles the error according to the contextutil.
    ///  1. If the `internalErr` is not `"pingcap/errors".Error`, or the error code is not
    ///     defined in the `errGroupMap`, or the error level is set to `LevelError`(0), the
    ///     `err` will be returned directly.
    ///  2. If the error level is set to `LevelWarn`, the `warnErr` will be appended as a
    ///     warning.
    ///  3. If the error level is set to `LevelIgnore`, this function will return a `nil`.
    ///
    /// In most cases, these three should be the same. If there are many different kinds of
    /// error internally, but they are expected to give the same error to users, the `err`
    /// can be different form `internalErr`. Also, if the warning is expected to be
    /// different from the initial error, you can also use the `warnErr` argument.
    // go: Context.HandleErrorWithAlias L1 [M-ERR]
    pub fn handle_error_with_alias(
        &self,
        internal_err: Error,
        err: Error,
        warn_err: Error,
    ) -> Result<(), Error> {
        let internal_err = tidb_base::errors::cause(&internal_err);
        if !internal_err.is_normalized() {
            return Err(err);
        }
        let Some(eg) = err_group_of(internal_err.code()) else {
            return Err(err);
        };
        match self.level_map[index(eg)] {
            LevelError => return Err(err),
            LevelWarn => self.append_warning(warn_err),
            _ => {}
        }
        Ok(())
    }
}

/// NewContext creates an error context to handle the errors and warnings
// go: NewContext L1
pub fn new_context(handler: WarnAppenderRef) -> Context {
    new_context_with_levels(LevelMap::default(), handler)
}

/// NewContextWithLevels creates an error context to handle the errors and warnings
// go: NewContextWithLevels L1
pub fn new_context_with_levels(levels: LevelMap, handler: WarnAppenderRef) -> Context {
    Context {
        warn_handler: Some(handler),
        level_map: levels,
    }
}

/// StrictNoWarningContext returns all errors directly, and ignore all errors
pub static StrictNoWarningContext: LazyLock<Context> =
    LazyLock::new(|| new_context(ignore_warn_appender()));

/// `errGroupMap`, built by Go's `init` from `group2Errors`: the group of each error code.
// go: init L1
fn err_group_of(code: tidb_base::errors::ErrCode) -> Option<ErrGroup> {
    let Ok(code) = u16::try_from(code) else {
        return None;
    };
    Some(match code {
        errno::ErrTruncatedWrongValue
        | errno::ErrDataTooLong
        | errno::ErrTruncatedWrongValueForField
        | errno::ErrWarnDataOutOfRange
        | errno::ErrDataOutOfRange
        | errno::ErrBadNumber
        | errno::ErrWrongValueForType
        | errno::ErrDatetimeFunctionOverflow
        | errno::WarnDataTruncated
        | errno::ErrIncorrectDatetimeValue => ErrGroupTruncate,
        errno::ErrBadNull | errno::ErrWarnNullToNotnull => ErrGroupBadNull,
        errno::ErrNoDefaultForField => ErrGroupNoDefault,
        errno::ErrDivisionByZero => ErrGroupDividedByZero,
        errno::ErrAutoincReadFailed => ErrGroupAutoIncReadFailed,
        errno::ErrNoPartitionForGivenValue | errno::ErrRowDoesNotMatchGivenPartitionSet => {
            ErrGroupNoMatchedPartition
        }
        errno::ErrDupEntry => ErrGroupDupKey,
        _ => return None,
    })
}

/// ResolveErrLevel resolves the error level according to the `ignore` and `warn` flags if
/// ignore is true, it will return `LevelIgnore` to ignore the error, otherwise, it will
/// return `LevelWarn` or `LevelError` according to the `warn` flag. Only one of `ignore`
/// and `warn` can be true.
// go: ResolveErrLevel L1
pub fn resolve_err_level(ignore: bool, warn: bool) -> Level {
    if ignore {
        return LevelIgnore;
    }
    if warn {
        return LevelWarn;
    }
    LevelError
}
