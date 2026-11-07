//! go-file: pkg/parser/types/eval_type.go

#![allow(non_upper_case_globals)]

/// EvalType indicates the specified types that arguments and result of a built-in function
/// should be.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EvalType(pub u8);

/// ETInt represents type INT in evaluation.
pub const ETInt: EvalType = EvalType(0);
/// ETReal represents type REAL in evaluation.
pub const ETReal: EvalType = EvalType(1);
/// ETDecimal represents type DECIMAL in evaluation.
pub const ETDecimal: EvalType = EvalType(2);
/// ETString represents type STRING in evaluation.
pub const ETString: EvalType = EvalType(3);
/// ETDatetime represents type DATETIME in evaluation.
pub const ETDatetime: EvalType = EvalType(4);
/// ETTimestamp represents type TIMESTAMP in evaluation.
pub const ETTimestamp: EvalType = EvalType(5);
/// ETDuration represents type DURATION in evaluation.
pub const ETDuration: EvalType = EvalType(6);
/// ETJson represents type JSON in evaluation.
pub const ETJson: EvalType = EvalType(7);
/// ETVectorFloat32 represents type VectorFloat32 in evaluation.
pub const ETVectorFloat32: EvalType = EvalType(8);

impl EvalType {
    /// IsStringKind returns true for ETString, ETDatetime, ETTimestamp, ETDuration, ETJson
    /// and ETVectorFloat32.
    // go: EvalType.IsStringKind L1
    pub fn is_string_kind(self) -> bool {
        self == ETString
            || self == ETDatetime
            || self == ETTimestamp
            || self == ETDuration
            || self == ETJson
            || self == ETVectorFloat32
    }

    /// IsVectorKind returns true for ETVectorXxx EvalTypes.
    // go: EvalType.IsVectorKind L1
    pub fn is_vector_kind(self) -> bool {
        self == ETVectorFloat32
    }

    /// String implements fmt.Stringer interface. Panics on an invalid value, as Go does.
    // go: EvalType.String L1 [M-NAME, M-PANIC]
    pub fn to_string(self) -> &'static str {
        match self {
            ETInt => "Int",
            ETReal => "Real",
            ETDecimal => "Decimal",
            ETString => "String",
            ETDatetime => "Datetime",
            ETTimestamp => "Timestamp",
            ETDuration => "Time",
            ETJson => "Json",
            ETVectorFloat32 => "VectorFloat32",
            EvalType(et) => panic!("invalid EvalType {et}"),
        }
    }
}
