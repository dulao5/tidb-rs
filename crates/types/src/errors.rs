//! go-file: pkg/types/errors.go
//!
//! The errors of `pkg/types/errors.go` as `tidb_base::Error` (D00.2, mapping rule M-ERR),
//! with Go's identifiers (M-GONAME). Go's sentinel comparisons `err == ErrTruncated` are
//! [`is_err`] / `ErrTruncated.equal(&e)` (`terror.Error.Equal`: same class and code).

pub use tidb_base::Error;

/// `err == proto` for a Go `error` held as `Result<(), Error>`: Go compares MyDecimal's
/// sentinel errors by identity; the sentinels are prototypes, so equality by class and code
/// (`terror.Error.Equal`) gives the same answer.
// glue: Go sentinel comparison `err == ErrX`
#[inline]
pub(crate) fn is_err(r: &Result<(), Error>, proto: &Error) -> bool {
    matches!(r, Err(e) if proto.equal(e))
}

/// `terror.Log(err)`: Go logs a non-nil error with its stack and continues.
// glue: terror.Log for a `Result<(), Error>`
pub(crate) fn terror_log(err: Result<(), Error>) {
    tidb_base::terror::log(err.err().as_ref());
}

#[allow(non_upper_case_globals)]
mod go_errors {
    use std::sync::LazyLock;

    use tidb_base::dbterror::{ClassExecutor, ClassTypes};
    use tidb_base::{Error, errno, mysql};

    /// The type names used in error messages.
    pub const DateTimeStr: &str = "datetime";
    /// The type names used in error messages.
    pub const DateStr: &str = "date";
    /// The type names used in error messages.
    pub const TimeStr: &str = "time";
    /// The type names used in error messages.
    pub const TimestampStr: &str = "timestamp";

    /// ErrTruncated is returned when data has been truncated during conversion.
    pub static ErrTruncated: LazyLock<Error> =
        LazyLock::new(|| ClassTypes.new_std(mysql::WarnDataTruncated));
    /// ErrOverflow is returned when data is out of range for a field type.
    pub static ErrOverflow: LazyLock<Error> =
        LazyLock::new(|| ClassTypes.new_std(mysql::ErrDataOutOfRange));
    /// ErrWarnDataOutOfRange is returned when the value in a numeric column that is outside the permissible range of the column data type.
    /// See https://dev.mysql.com/doc/refman/5.5/en/out-of-range-and-overflow.html for details
    pub static ErrWarnDataOutOfRange: LazyLock<Error> =
        LazyLock::new(|| ClassTypes.new_std(mysql::ErrWarnDataOutOfRange));
    /// ErrDatetimeFunctionOverflow is returned when the calculation in datetime function cause overflow.
    pub static ErrDatetimeFunctionOverflow: LazyLock<Error> =
        LazyLock::new(|| ClassTypes.new_std(mysql::ErrDatetimeFunctionOverflow));
    /// ErrInvalidYearFormat is returned when the input is not a valid year format.
    pub static ErrInvalidYearFormat: LazyLock<Error> =
        LazyLock::new(|| ClassTypes.new_std(errno::ErrInvalidYearFormat));
    /// ErrTruncatedWrongVal is returned when data has been truncated during conversion.
    pub static ErrTruncatedWrongVal: LazyLock<Error> =
        LazyLock::new(|| ClassTypes.new_std(mysql::ErrTruncatedWrongValue));
    /// ErrWrongValue is returned when the input value is in wrong format.
    pub static ErrWrongValue: LazyLock<Error> = LazyLock::new(|| {
        ClassTypes.new_std_err(
            mysql::ErrTruncatedWrongValue,
            mysql::MySQLErrName
                .get(&mysql::ErrWrongValue)
                .expect("ErrWrongValue has a message"),
        )
    });
    /// ErrIncorrectDatetimeValue is returned when the input is not valid date time value.
    pub static ErrIncorrectDatetimeValue: LazyLock<Error> =
        LazyLock::new(|| ClassTypes.new_std(errno::ErrIncorrectDatetimeValue));
    /// ErrJSONBadOneOrAllArg is returned when the one_or_all argument isn't 'one' or 'all'.
    pub static ErrJSONBadOneOrAllArg: LazyLock<Error> =
        LazyLock::new(|| ClassTypes.new_std(errno::ErrJSONBadOneOrAllArg));
    /// ErrJSONVacuousPath is returned when path is empty
    pub static ErrJSONVacuousPath: LazyLock<Error> =
        LazyLock::new(|| ClassTypes.new_std(errno::ErrJSONVacuousPath));
    /// ErrDataTooLong is returned when converts a string value that is longer than field type length.
    pub static ErrDataTooLong: LazyLock<Error> =
        LazyLock::new(|| ClassTypes.new_std(mysql::ErrDataTooLong));
    /// ErrIllegalValueForType is returned when value of type is illegal.
    pub static ErrIllegalValueForType: LazyLock<Error> =
        LazyLock::new(|| ClassTypes.new_std(mysql::ErrIllegalValueForType));
    /// ErrDivByZero is return when do division by 0.
    pub static ErrDivByZero: LazyLock<Error> =
        LazyLock::new(|| ClassTypes.new_std(mysql::ErrDivisionByZero));
    /// ErrTooBigDisplayWidth is return when display width out of range for column.
    pub static ErrTooBigDisplayWidth: LazyLock<Error> =
        LazyLock::new(|| ClassTypes.new_std(mysql::ErrTooBigDisplaywidth));
    /// ErrTooBigFieldLength is return when column length too big for column.
    pub static ErrTooBigFieldLength: LazyLock<Error> =
        LazyLock::new(|| ClassTypes.new_std(mysql::ErrTooBigFieldlength));
    /// ErrTooBigSet is returned when too many strings for column.
    pub static ErrTooBigSet: LazyLock<Error> =
        LazyLock::new(|| ClassTypes.new_std(mysql::ErrTooBigSet));
    /// ErrTooBigScale is returned when type DECIMAL/NUMERIC scale is bigger than mysql.MaxDecimalScale.
    pub static ErrTooBigScale: LazyLock<Error> =
        LazyLock::new(|| ClassTypes.new_std(mysql::ErrTooBigScale));
    /// ErrTooBigPrecision is returned when type DECIMAL/NUMERIC or DATETIME precision is bigger than mysql.MaxDecimalWidth or types.MaxFsp
    pub static ErrTooBigPrecision: LazyLock<Error> =
        LazyLock::new(|| ClassTypes.new_std(mysql::ErrTooBigPrecision));
    /// ErrBadNumber is return when parsing an invalid binary decimal number.
    pub static ErrBadNumber: LazyLock<Error> =
        LazyLock::new(|| ClassTypes.new_std(errno::ErrBadNumber));
    /// ErrInvalidFieldSize is returned when the precision of a column is out of range.
    pub static ErrInvalidFieldSize: LazyLock<Error> =
        LazyLock::new(|| ClassTypes.new_std(mysql::ErrInvalidFieldSize));
    /// ErrMBiggerThanD is returned when precision less than the scale.
    pub static ErrMBiggerThanD: LazyLock<Error> =
        LazyLock::new(|| ClassTypes.new_std(mysql::ErrMBiggerThanD));
    /// ErrDuplicatedValueInType is returned when enum column has duplicated value.
    pub static ErrDuplicatedValueInType: LazyLock<Error> =
        LazyLock::new(|| ClassTypes.new_std(mysql::ErrDuplicatedValueInType));
    /// ErrCastAsSignedOverflow is returned when positive out-of-range integer, and convert to it's negative complement.
    pub static ErrCastAsSignedOverflow: LazyLock<Error> =
        LazyLock::new(|| ClassTypes.new_std(errno::ErrCastAsSignedOverflow));
    /// ErrCastNegIntAsUnsigned is returned when a negative integer be casted to an unsigned int.
    pub static ErrCastNegIntAsUnsigned: LazyLock<Error> =
        LazyLock::new(|| ClassTypes.new_std(errno::ErrCastNegIntAsUnsigned));
    /// ErrInvalidYear is returned when the input value is not a valid year.
    pub static ErrInvalidYear: LazyLock<Error> =
        LazyLock::new(|| ClassTypes.new_std(errno::ErrInvalidYear));
    /// ErrInvalidWeekModeFormat is returned when the week mode is wrong.
    pub static ErrInvalidWeekModeFormat: LazyLock<Error> =
        LazyLock::new(|| ClassTypes.new_std(errno::ErrInvalidWeekModeFormat));
    /// ErrWrongFieldSpec is returned when the column specifier incorrect.
    pub static ErrWrongFieldSpec: LazyLock<Error> =
        LazyLock::new(|| ClassTypes.new_std(mysql::ErrWrongFieldSpec));
    /// ErrPartitionStatsMissing is returned when the partition-level stats is missing and the build global-level stats fails.
    pub static ErrPartitionStatsMissing: LazyLock<Error> =
        LazyLock::new(|| ClassTypes.new_std(errno::ErrPartitionStatsMissing));
    /// ErrPartitionColumnStatsMissing is returned when the partition-level column stats is missing and the build global-level stats fails.
    pub static ErrPartitionColumnStatsMissing: LazyLock<Error> =
        LazyLock::new(|| ClassTypes.new_std(errno::ErrPartitionColumnStatsMissing));
    /// ErrSyntax (`ClassTypes.NewStdErr(mysql.ErrParse, mysql.MySQLErrName[mysql.ErrSyntax])`).
    pub static ErrSyntax: LazyLock<Error> = LazyLock::new(|| {
        ClassTypes.new_std_err(
            mysql::ErrParse,
            mysql::MySQLErrName
                .get(&mysql::ErrSyntax)
                .expect("ErrSyntax has a message"),
        )
    });
    /// ErrWrongValue2 (`ClassTypes.NewStdErr(mysql.ErrWrongValue, mysql.MySQLErrName[mysql.ErrWrongValue])`).
    pub static ErrWrongValue2: LazyLock<Error> = LazyLock::new(|| {
        ClassTypes.new_std_err(
            mysql::ErrWrongValue,
            mysql::MySQLErrName
                .get(&mysql::ErrWrongValue)
                .expect("ErrWrongValue has a message"),
        )
    });
    /// ErrWrongValueForType (`ClassTypes.NewStdErr(mysql.ErrWrongValueForType, mysql.MySQLErrName[mysql.ErrWrongValueForType])`).
    pub static ErrWrongValueForType: LazyLock<Error> = LazyLock::new(|| {
        ClassTypes.new_std_err(
            mysql::ErrWrongValueForType,
            mysql::MySQLErrName
                .get(&mysql::ErrWrongValueForType)
                .expect("ErrWrongValueForType has a message"),
        )
    });
    /// ErrTimestampInDSTTransition is returned if the timestamp is in a Daylight Saving Time transition
    pub static ErrTimestampInDSTTransition: LazyLock<Error> =
        LazyLock::new(|| ClassExecutor.new_std(errno::ErrTimeStampInDSTTransition));
}

pub use go_errors::*;
