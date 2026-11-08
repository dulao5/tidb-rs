//! go-file: pkg/sessionctx/variable/error.go
//!
//! The errors of the variable package as `tidb_base::Error` prototypes (M-ERR), with Go's
//! identifiers (M-GONAME).

#![allow(non_upper_case_globals)]

use std::sync::LazyLock;

use tidb_base::Error;
use tidb_base::dbterror::{ClassExecutor, ClassVariable};
use tidb_base::errno;
use tidb_base::mysql;

macro_rules! std_err {
    ($(#[$m:meta])* $vis:vis $name:ident = $class:ident, $code:ident) => {
        $(#[$m])*
        $vis static $name: LazyLock<Error> = LazyLock::new(|| $class.new_std(errno::$code));
    };
}

std_err!(pub(crate) errWarnDeprecatedSyntax = ClassVariable, ErrWarnDeprecatedSyntax);
std_err!(/// ErrSnapshotTooOld.
    pub ErrSnapshotTooOld = ClassVariable, ErrSnapshotTooOld);
std_err!(/// ErrUnsupportedValueForVar.
    pub ErrUnsupportedValueForVar = ClassVariable, ErrUnsupportedValueForVar);
std_err!(/// ErrNotSupportedYet (dbterror.ErrNotSupportedYet; returned by `SET GLOBAL` in the
    /// read-only phase, owner issue #58).
    pub ErrNotSupportedYet = ClassVariable, ErrNotSupportedYet);
std_err!(/// ErrUnknownSystemVar.
    pub ErrUnknownSystemVar = ClassVariable, ErrUnknownSystemVariable);
std_err!(/// ErrIncorrectScope.
    pub ErrIncorrectScope = ClassVariable, ErrIncorrectGlobalLocalVar);
std_err!(/// ErrUnknownTimeZone.
    pub ErrUnknownTimeZone = ClassVariable, ErrUnknownTimeZone);
std_err!(/// ErrReadOnly.
    pub ErrReadOnly = ClassVariable, ErrVariableIsReadonly);
std_err!(/// ErrWrongValueForVar.
    pub ErrWrongValueForVar = ClassVariable, ErrWrongValueForVar);
std_err!(/// ErrWrongTypeForVar.
    pub ErrWrongTypeForVar = ClassVariable, ErrWrongTypeForVar);
std_err!(/// ErrTruncatedWrongValue.
    pub ErrTruncatedWrongValue = ClassVariable, ErrTruncatedWrongValue);
std_err!(/// ErrMaxPreparedStmtCountReached.
    pub ErrMaxPreparedStmtCountReached = ClassVariable, ErrMaxPreparedStmtCountReached);
std_err!(/// ErrUnsupportedIsolationLevel.
    pub ErrUnsupportedIsolationLevel = ClassVariable, ErrUnsupportedIsolationLevel);
std_err!(pub(crate) errUnknownSystemVariable = ClassVariable, ErrUnknownSystemVariable);
std_err!(pub(crate) errGlobalVariable = ClassVariable, ErrGlobalVariable);
std_err!(pub(crate) errLocalVariable = ClassVariable, ErrLocalVariable);
std_err!(/// ErrNotValidPassword.
    pub ErrNotValidPassword = ClassExecutor, ErrNotValidPassword);
std_err!(/// ErrVariableNoLongerSupported.
    pub ErrVariableNoLongerSupported = ClassVariable, ErrVariableNoLongerSupported);
std_err!(/// ErrInvalidDefaultUTF8MB4Collation.
    pub ErrInvalidDefaultUTF8MB4Collation = ClassVariable, ErrInvalidDefaultUTF8MB4Collation);

pub(crate) static errValueNotSupportedWhen: LazyLock<Error> = LazyLock::new(|| {
    static M: mysql::ErrMessage = mysql::message("%s = OFF is not supported when %s = ON", &[]);
    ClassVariable.new_std_err(errno::ErrNotSupportedYet, &M)
});

/// ErrFunctionsNoopImpl is an error to say the behavior is protected by the tidb_enable_noop_functions sysvar.
/// This is copied from expression.ErrFunctionsNoopImpl to prevent circular dependencies.
/// It needs to be public for tests.
pub static ErrFunctionsNoopImpl: LazyLock<Error> = LazyLock::new(|| {
    static M: mysql::ErrMessage = mysql::message(
        "function %s has only noop implementation in tidb now, use tidb_enable_noop_functions to enable these functions",
        &[],
    );
    ClassVariable.new_std_err(errno::ErrNotSupportedYet, &M)
});

/// ErrWarnDeprecatedSyntaxNoReplacement.
pub static ErrWarnDeprecatedSyntaxNoReplacement: LazyLock<Error> = LazyLock::new(|| {
    static M: mysql::ErrMessage = mysql::message(
        "Updating '%s' is deprecated. It will be made read-only in a future release.",
        &[],
    );
    ClassVariable.new_std_err(errno::ErrWarnDeprecatedSyntaxNoReplacement, &M)
});

/// ErrWarnDeprecatedSyntaxSimpleMsg.
pub static ErrWarnDeprecatedSyntaxSimpleMsg: LazyLock<Error> = LazyLock::new(|| {
    static M: mysql::ErrMessage = mysql::message(
        "%s is deprecated and will be removed in a future release.",
        &[],
    );
    ClassVariable.new_std_err(errno::ErrWarnDeprecatedSyntaxNoReplacement, &M)
});
