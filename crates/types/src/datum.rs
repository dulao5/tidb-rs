//! The Datum kinds of `pkg/types/datum.go` (constants only, so far: the `Datum` type and
//! its methods follow in the second part of block D01.1).

#![allow(non_upper_case_globals)]

/// Kind constants.
pub const KindNull: u8 = 0;
pub const KindInt64: u8 = 1;
pub const KindUint64: u8 = 2;
pub const KindFloat32: u8 = 3;
pub const KindFloat64: u8 = 4;
pub const KindString: u8 = 5;
pub const KindBytes: u8 = 6;
/// Used for BIT / HEX literals.
pub const KindBinaryLiteral: u8 = 7;
pub const KindMysqlDecimal: u8 = 8;
pub const KindMysqlDuration: u8 = 9;
pub const KindMysqlEnum: u8 = 10;
/// Used for BIT table column values.
pub const KindMysqlBit: u8 = 11;
pub const KindMysqlSet: u8 = 12;
pub const KindMysqlTime: u8 = 13;
pub const KindInterface: u8 = 14;
pub const KindMinNotNull: u8 = 15;
pub const KindMaxValue: u8 = 16;
pub const KindRaw: u8 = 17;
pub const KindMysqlJSON: u8 = 18;
pub const KindVectorFloat32: u8 = 19;
