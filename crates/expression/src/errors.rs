//! go-file: pkg/expression/errors.go
//!
//! The errors of package `expression` (M-ERR, Go identifiers per M-GONAME) and the three
//! `handle*` helpers that decide between an error and a warning.

#![allow(non_upper_case_globals)]

use std::sync::LazyLock;

use tidb_base::dbterror::ClassExpression;
use tidb_base::gofmt::Arg;
use tidb_base::mysql::message;
use tidb_base::{Error, errno};
use tidb_types::errors::{
    ErrDatetimeFunctionOverflow, ErrIncorrectDatetimeValue, ErrInvalidWeekModeFormat,
    ErrTruncatedWrongVal, ErrWrongValue, ErrWrongValueForType,
};

use crate::EvalContext;
use crate::context::{err_ctx, type_ctx};

macro_rules! std_errors {
    ($($(#[$m:meta])* $vis:vis $name:ident = $code:ident;)*) => {$(
        $(#[$m])*
        $vis static $name: LazyLock<Error> = LazyLock::new(|| ClassExpression.new_std(errno::$code));
    )*};
}

std_errors! {
    /// ErrIncorrectParameterCount.
    pub ErrIncorrectParameterCount = ErrWrongParamcountToNativeFct;
    /// ErrDivisionByZero.
    pub ErrDivisionByZero = ErrDivisionByZero;
    /// ErrRegexp.
    pub ErrRegexp = ErrRegexp;
    /// ErrOperandColumns.
    pub ErrOperandColumns = ErrOperandColumns;
    /// ErrCutValueGroupConcat.
    pub ErrCutValueGroupConcat = ErrCutValueGroupConcat;
    /// ErrInvalidArgumentForLogarithm.
    pub ErrInvalidArgumentForLogarithm = ErrInvalidArgumentForLogarithm;
    /// ErrIncorrectType.
    pub ErrIncorrectType = ErrIncorrectType;
    /// ErrInvalidTypeForJSON.
    pub ErrInvalidTypeForJSON = ErrInvalidTypeForJSON;
    /// ErrInvalidTableSample.
    pub ErrInvalidTableSample = ErrInvalidTableSample;
    /// ErrNotSupportedYet.
    pub ErrNotSupportedYet = ErrNotSupportedYet;
    /// ErrInvalidJSONForFuncIndex.
    pub ErrInvalidJSONForFuncIndex = ErrInvalidJSONValueForFuncIndex;
    /// ErrDataOutOfRangeFuncIndex.
    pub ErrDataOutOfRangeFuncIndex = ErrDataOutOfRangeFunctionalIndex;
    /// ErrFuncIndexDataIsTooLong.
    pub ErrFuncIndexDataIsTooLong = ErrFunctionalIndexDataIsTooLong;
    /// ErrFunctionNotExists.
    pub ErrFunctionNotExists = ErrSpDoesNotExist;

    /// errZlibZData (unexported in Go; used by the built-in functions).
    pub errZlibZData = ErrZlibZData;
    /// errZlibZBuf (unexported in Go; used by the built-in functions).
    pub errZlibZBuf = ErrZlibZBuf;
    /// errIncorrectArgs (unexported in Go; used by the built-in functions).
    pub errIncorrectArgs = ErrWrongArguments;
    /// errUnknownCharacterSet (unexported in Go; used by the built-in functions).
    pub errUnknownCharacterSet = ErrUnknownCharacterSet;
    /// errDeprecatedSyntaxNoReplacement (unexported in Go; used by the built-in functions).
    pub errDeprecatedSyntaxNoReplacement = ErrWarnDeprecatedSyntaxNoReplacement;
    /// errWarnAllowedPacketOverflowed (unexported in Go; used by the built-in functions).
    pub errWarnAllowedPacketOverflowed = ErrWarnAllowedPacketOverflowed;
    /// errWarnOptionIgnored (unexported in Go; used by the built-in functions).
    pub errWarnOptionIgnored = WarnOptionIgnored;
    /// errTruncatedWrongValue (unexported in Go; used by the built-in functions).
    pub errTruncatedWrongValue = ErrTruncatedWrongValue;
    /// errUnknownLocale (unexported in Go; used by the built-in functions).
    pub errUnknownLocale = ErrUnknownLocale;
    /// errNonUniq (unexported in Go; used by the built-in functions).
    pub errNonUniq = ErrNonUniq;
    /// errWrongValueForType (unexported in Go; used by the built-in functions).
    pub errWrongValueForType = ErrWrongValueForType;
    /// errUnknown (unexported in Go; used by the built-in functions).
    pub errUnknown = ErrUnknown;
    /// errSpecificAccessDenied (unexported in Go; used by the built-in functions).
    pub errSpecificAccessDenied = ErrSpecificAccessDenied;
    /// errUserLockDeadlock (unexported in Go; used by the built-in functions).
    pub errUserLockDeadlock = ErrUserLockDeadlock;
    /// errUserLockWrongName (unexported in Go; used by the built-in functions).
    pub errUserLockWrongName = ErrUserLockWrongName;
    /// errJSONInBooleanContext (unexported in Go; used by the built-in functions).
    pub errJSONInBooleanContext = ErrJSONInBooleanContext;
    /// errBadNull (unexported in Go; used by the built-in functions).
    pub errBadNull = ErrBadNull;
    /// errSequenceAccessDenied (unexported in Go; used by the built-in functions).
    pub errSequenceAccessDenied = ErrTableaccessDenied;
}

/// ErrFunctionsNoopImpl.
pub static ErrFunctionsNoopImpl: LazyLock<Error> = LazyLock::new(|| {
    ClassExpression.new_std_err(
        errno::ErrNotSupportedYet,
        &message(
            "function %s has only noop implementation in tidb now, use tidb_enable_noop_functions to enable these functions",
            &[],
        ),
    )
});

/// errDefaultValue.
pub static errDefaultValue: LazyLock<Error> = LazyLock::new(|| {
    ClassExpression.new_std_err(
        errno::ErrInvalidDefault,
        &message("invalid default value", &[]),
    )
});

/// errUnsupportedJSONComparison.
pub static errUnsupportedJSONComparison: LazyLock<Error> = LazyLock::new(|| {
    ClassExpression.new_std_err(
        errno::ErrNotSupportedYet,
        &message(
            "comparison of JSON in the LEAST and GREATEST operators",
            &[],
        ),
    )
});

/// handleInvalidTimeError reports error or warning depend on the context.
// go: handleInvalidTimeError L1
pub fn handle_invalid_time_error(ctx: &dyn EvalContext, err: Error) -> Result<(), Error> {
    if !(ErrWrongValue.equal(&err)
        || ErrWrongValueForType.equal(&err)
        || ErrTruncatedWrongVal.equal(&err)
        || ErrInvalidWeekModeFormat.equal(&err)
        || ErrDatetimeFunctionOverflow.equal(&err)
        || ErrIncorrectDatetimeValue.equal(&err))
    {
        return Err(err);
    }
    let ec = err_ctx(ctx);
    ec.handle_error(err)
}

/// handleDivisionByZeroError reports error or warning depend on the context.
// go: handleDivisionByZeroError L1
pub fn handle_division_by_zero_error(ctx: &dyn EvalContext) -> Result<(), Error> {
    let ec = err_ctx(ctx);
    ec.handle_error(ErrDivisionByZero.clone())
}

/// handleAllowedPacketOverflowed reports error or warning depend on the context.
// go: handleAllowedPacketOverflowed L1
pub fn handle_allowed_packet_overflowed(
    ctx: &dyn EvalContext,
    expr_name: &str,
    max_allowed_packet_size: u64,
) -> Result<(), Error> {
    let err = errWarnAllowedPacketOverflowed.fast_gen_by_args(vec![
        Arg::from(expr_name.to_string()),
        Arg::from(max_allowed_packet_size),
    ]);
    let tc = type_ctx(ctx);
    let f = tc.flags();
    if f.truncate_as_warning() || f.ignore_truncate_err() {
        tc.append_warning(err);
        return Ok(());
    }
    Err(err)
}
