//! go-file: pkg/types/context.go
//!
//! `types.Context`: the flags, time zone and warning sink of type conversions. The file
//! belongs to block D01.1; it is ported whole ahead of that block because the time code
//! (D01.3) takes a `Context` everywhere (constitution §4.5: no out-edges beyond tidb-base).

#![allow(non_upper_case_globals)]

use std::sync::{Arc, LazyLock};

use tidb_base::Error;
use tidb_base::contextutil::WarnAppender;
use tidb_base::gostd::time::{Location, utc};

/// StrictFlags is a flags with the fields unset.
pub const StrictFlags: Flags = Flags(0);

/// Flags indicate how to handle the conversion of a value.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Flags(pub u16);

/// FlagIgnoreTruncateErr indicates to ignore the truncate error.
/// If this flag is set, `FlagTruncateAsWarning` will be ignored.
pub const FlagIgnoreTruncateErr: Flags = Flags(1 << 0);
/// FlagTruncateAsWarning indicates to append the truncate error to warnings instead of returning it to user.
pub const FlagTruncateAsWarning: Flags = Flags(1 << 1);
/// FlagAllowNegativeToUnsigned indicates to allow the casting from negative to unsigned int.
pub const FlagAllowNegativeToUnsigned: Flags = Flags(1 << 2);
/// FlagIgnoreZeroDateErr indicates to ignore the zero-date error.
pub const FlagIgnoreZeroDateErr: Flags = Flags(1 << 3);
/// FlagIgnoreZeroInDateErr indicates to ignore the zero-in-date error.
pub const FlagIgnoreZeroInDateErr: Flags = Flags(1 << 4);
/// FlagIgnoreInvalidDateErr indicates to ignore the invalid-date error.
pub const FlagIgnoreInvalidDateErr: Flags = Flags(1 << 5);
/// FlagSkipASCIICheck indicates to skip the ASCII check when converting the value to an ASCII string.
pub const FlagSkipASCIICheck: Flags = Flags(1 << 6);
/// FlagSkipUTF8Check indicates to skip the UTF8 check when converting the value to an UTF8MB3 string.
pub const FlagSkipUTF8Check: Flags = Flags(1 << 7);
/// FlagSkipUTF8MB4Check indicates to skip the UTF8MB4 check when converting the value to an UTF8 string.
pub const FlagSkipUTF8MB4Check: Flags = Flags(1 << 8);
/// FlagCastTimeToYearThroughConcat indicates to cast time to year through concatenation.
/// For example, `00:19:59` will be converted to '1959'
pub const FlagCastTimeToYearThroughConcat: Flags = Flags(1 << 9);

// glue: Go `f | flag` / `f &^ flag` on Flags
fn with(f: Flags, flag: Flags, set: bool) -> Flags {
    if set {
        Flags(f.0 | flag.0)
    } else {
        Flags(f.0 & !flag.0)
    }
}

impl Flags {
    /// AllowNegativeToUnsigned indicates whether the flag `FlagAllowNegativeToUnsigned` is set
    // go: Flags.AllowNegativeToUnsigned L1
    pub fn allow_negative_to_unsigned(self) -> bool {
        self.0 & FlagAllowNegativeToUnsigned.0 != 0
    }

    /// WithAllowNegativeToUnsigned returns a new flags with `FlagAllowNegativeToUnsigned` set/unset according to the clip parameter
    // go: Flags.WithAllowNegativeToUnsigned L1
    pub fn with_allow_negative_to_unsigned(self, clip: bool) -> Flags {
        with(self, FlagAllowNegativeToUnsigned, clip)
    }

    /// SkipASCIICheck indicates whether the flag `FlagSkipASCIICheck` is set
    // go: Flags.SkipASCIICheck L1
    pub fn skip_ascii_check(self) -> bool {
        self.0 & FlagSkipASCIICheck.0 != 0
    }

    /// WithSkipSACIICheck returns a new flags with `FlagSkipASCIICheck` set/unset according to the skip parameter
    // go: Flags.WithSkipSACIICheck L1
    pub fn with_skip_sacii_check(self, skip: bool) -> Flags {
        with(self, FlagSkipASCIICheck, skip)
    }

    /// SkipUTF8Check indicates whether the flag `FlagSkipUTF8Check` is set
    // go: Flags.SkipUTF8Check L1
    pub fn skip_utf8_check(self) -> bool {
        self.0 & FlagSkipUTF8Check.0 != 0
    }

    /// WithSkipUTF8Check returns a new flags with `FlagSkipUTF8Check` set/unset according to the skip parameter
    // go: Flags.WithSkipUTF8Check L1
    pub fn with_skip_utf8_check(self, skip: bool) -> Flags {
        with(self, FlagSkipUTF8Check, skip)
    }

    /// SkipUTF8MB4Check indicates whether the flag `FlagSkipUTF8MB4Check` is set
    // go: Flags.SkipUTF8MB4Check L1
    pub fn skip_utf8mb4_check(self) -> bool {
        self.0 & FlagSkipUTF8MB4Check.0 != 0
    }

    /// WithSkipUTF8MB4Check returns a new flags with `FlagSkipUTF8MB4Check` set/unset according to the skip parameter
    // go: Flags.WithSkipUTF8MB4Check L1
    pub fn with_skip_utf8mb4_check(self, skip: bool) -> Flags {
        with(self, FlagSkipUTF8MB4Check, skip)
    }

    /// IgnoreTruncateErr indicates whether the flag `FlagIgnoreTruncateErr` is set
    // go: Flags.IgnoreTruncateErr L1
    pub fn ignore_truncate_err(self) -> bool {
        self.0 & FlagIgnoreTruncateErr.0 != 0
    }

    /// WithIgnoreTruncateErr returns a new flags with `FlagIgnoreTruncateErr` set/unset according to the skip parameter
    // go: Flags.WithIgnoreTruncateErr L1
    pub fn with_ignore_truncate_err(self, ignore: bool) -> Flags {
        with(self, FlagIgnoreTruncateErr, ignore)
    }

    /// TruncateAsWarning indicates whether the flag `FlagTruncateAsWarning` is set
    // go: Flags.TruncateAsWarning L1
    pub fn truncate_as_warning(self) -> bool {
        self.0 & FlagTruncateAsWarning.0 != 0
    }

    /// WithTruncateAsWarning returns a new flags with `FlagTruncateAsWarning` set/unset according to the skip parameter
    // go: Flags.WithTruncateAsWarning L1
    pub fn with_truncate_as_warning(self, warn: bool) -> Flags {
        with(self, FlagTruncateAsWarning, warn)
    }

    /// IgnoreZeroInDate indicates whether the flag `FlagIgnoreZeroInData` is set
    // go: Flags.IgnoreZeroInDate L1
    pub fn ignore_zero_in_date(self) -> bool {
        self.0 & FlagIgnoreZeroInDateErr.0 != 0
    }

    /// WithIgnoreZeroInDate returns a new flags with `FlagIgnoreZeroInDateErr` set/unset according to the ignore parameter
    // go: Flags.WithIgnoreZeroInDate L1
    pub fn with_ignore_zero_in_date(self, ignore: bool) -> Flags {
        with(self, FlagIgnoreZeroInDateErr, ignore)
    }

    /// IgnoreInvalidDateErr indicates whether the flag `FlagIgnoreInvalidDateErr` is set
    // go: Flags.IgnoreInvalidDateErr L1
    pub fn ignore_invalid_date_err(self) -> bool {
        self.0 & FlagIgnoreInvalidDateErr.0 != 0
    }

    /// WithIgnoreInvalidDateErr returns a new flags with `FlagIgnoreInvalidDateErr` set/unset according to the ignore parameter
    // go: Flags.WithIgnoreInvalidDateErr L1
    pub fn with_ignore_invalid_date_err(self, ignore: bool) -> Flags {
        with(self, FlagIgnoreInvalidDateErr, ignore)
    }

    /// IgnoreZeroDateErr indicates whether the flag `FlagIgnoreZeroDateErr` is set
    // go: Flags.IgnoreZeroDateErr L1
    pub fn ignore_zero_date_err(self) -> bool {
        self.0 & FlagIgnoreZeroDateErr.0 != 0
    }

    /// WithIgnoreZeroDateErr returns a new flags with `FlagIgnoreZeroDateErr` set/unset according to the ignore parameter
    // go: Flags.WithIgnoreZeroDateErr L1
    pub fn with_ignore_zero_date_err(self, ignore: bool) -> Flags {
        with(self, FlagIgnoreZeroDateErr, ignore)
    }

    /// CastTimeToYearThroughConcat whether `FlagCastTimeToYearThroughConcat` is set
    // go: Flags.CastTimeToYearThroughConcat L1
    pub fn cast_time_to_year_through_concat(self) -> bool {
        self.0 & FlagCastTimeToYearThroughConcat.0 != 0
    }

    /// WithCastTimeToYearThroughConcat sets `FlagCastTimeToYearThroughConcat`
    // go: Flags.WithCastTimeToYearThroughConcat L1
    pub fn with_cast_time_to_year_through_concat(self, flag: bool) -> Flags {
        with(self, FlagCastTimeToYearThroughConcat, flag)
    }
}

/// The warning sink of a `Context` (Go: `contextutil.WarnAppender`, an interface value).
pub type WarnHandler = Arc<dyn WarnAppender + Send + Sync>;

/// Context provides the information when converting between different types.
#[derive(Clone)]
pub struct Context {
    flags: Flags,
    loc: Arc<Location>,
    warn_handler: WarnHandler,
}

impl std::fmt::Debug for Context {
    // glue: Debug for a struct holding a trait object
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Context")
            .field("flags", &self.flags)
            .field("loc", &self.loc.string())
            .finish()
    }
}

/// NewContext creates a new `Context`
// go: NewContext L1
pub fn new_context(flags: Flags, loc: Arc<Location>, handler: WarnHandler) -> Context {
    Context {
        flags,
        loc,
        warn_handler: handler,
    }
}

impl Context {
    /// Flags returns the flags of the context
    // go: Context.Flags L1
    pub fn flags(&self) -> Flags {
        self.flags
    }

    /// WithFlags returns a new context with the flags set to the given value
    // go: Context.WithFlags L1
    pub fn with_flags(&self, f: Flags) -> Context {
        let mut ctx = self.clone();
        ctx.flags = f;
        ctx
    }

    /// WithLocation returns a new context with the given location
    // go: Context.WithLocation L1
    pub fn with_location(&self, loc: Arc<Location>) -> Context {
        let mut ctx = self.clone();
        ctx.loc = loc;
        ctx
    }

    /// Location returns the location of the context
    // The location is never nil: the Rust Context always holds one.
    // go: Context.Location L1
    pub fn location(&self) -> Arc<Location> {
        Arc::clone(&self.loc)
    }

    /// AppendWarning appends the error to warning.
    // go: Context.AppendWarning L1
    pub fn append_warning(&self, err: Error) {
        self.warn_handler.append_warning(err);
    }
}

/// `contextutil.IgnoreWarn`: a WarnAppender that drops every warning.
// glue: contextutil.IgnoreWarn (block D00.1), the handler of the default contexts below
#[derive(Debug, Default)]
pub struct IgnoreWarn;

impl WarnAppender for IgnoreWarn {
    // glue: contextutil.ignoreWarn.AppendWarning
    fn append_warning(&self, _err: Error) {}
    // glue: contextutil.ignoreWarn.AppendNote
    fn append_note(&self, _msg: Error) {}
}

/// DefaultStmtFlags is the default flags for statement context with the flag `FlagAllowNegativeToUnsigned` set.
pub const DefaultStmtFlags: Flags =
    Flags(StrictFlags.0 | FlagAllowNegativeToUnsigned.0 | FlagIgnoreZeroDateErr.0);

/// DefaultStmtNoWarningContext is the context with default statement flags without any other special configuration
pub static DefaultStmtNoWarningContext: LazyLock<Context> =
    LazyLock::new(|| new_context(DefaultStmtFlags, utc(), Arc::new(IgnoreWarn)));

/// StrictContext is the most strict context which returns every error it meets
pub static StrictContext: LazyLock<Context> =
    LazyLock::new(|| new_context(StrictFlags, utc(), Arc::new(IgnoreWarn)));
