//! Datums to protocol values. The protocol layer picks the text/binary encoding from the
//! column type; here each datum becomes the value kind matching its result type.

use tidb_distsql::Datum;
use tidb_meta::model::FieldType;
use tidb_meta::model::tp;
use tidb_mysql_protocol::DateTime;
use tidb_mysql_protocol::Duration;
use tidb_mysql_protocol::SqlError;
use tidb_mysql_protocol::Value;

/// Converts one datum of result type `ft`. Stored CHAR values are returned as stored: TiDB
/// strips trailing spaces of non-binary CHAR on write (`table.CastValue`) and pads BINARY(n)
/// with zero bytes there, and does not change them on read (no PAD_CHAR_TO_FULL_LENGTH).
pub fn to_value(d: Datum, ft: &FieldType) -> Result<Value, SqlError> {
    Ok(match d {
        Datum::Null => Value::Null,
        Datum::I64(i) => {
            if ft.is_unsigned() {
                Value::UInt(i as u64)
            } else {
                Value::Int(i)
            }
        }
        Datum::U64(u) => {
            if ft.is_unsigned() {
                Value::UInt(u)
            } else {
                Value::Int(u as i64)
            }
        }
        Datum::F64(f) => {
            if ft.tp == tp::FLOAT {
                Value::Float(f as f32)
            } else {
                Value::Double(f)
            }
        }
        Datum::Dec(d) => Value::Decimal(d.to_string()),
        Datum::Bytes(b) => Value::Bytes(b),
        Datum::Time(t) => Value::DateTime(DateTime {
            year: t.year() as u16,
            month: t.month() as u8,
            day: t.day() as u8,
            hour: t.hour() as u8,
            minute: t.minute() as u8,
            second: t.second() as u8,
            microsecond: t.micro(),
        }),
        Datum::Dur(d) => Value::Time(Duration {
            negative: d.is_neg(),
            hours: d.hours(),
            minutes: d.minutes() as u8,
            seconds: d.secs() as u8,
            microseconds: d.subsec_micros(),
        }),
        other => {
            return Err(SqlError::not_supported(format!(
                "returning value {other:?}"
            )));
        }
    })
}

#[cfg(test)]
mod tests {
    use tidb_meta::model::flag;

    use super::*;

    fn ft(t: u8, flags: u32) -> FieldType {
        FieldType {
            tp: t,
            flag: flags,
            ..FieldType::default()
        }
    }

    #[test]
    fn values() {
        assert_eq!(
            to_value(Datum::I64(-3), &ft(tp::LONG, 0)).unwrap(),
            Value::Int(-3)
        );
        assert_eq!(
            to_value(Datum::U64(u64::MAX), &ft(tp::LONGLONG, flag::UNSIGNED)).unwrap(),
            Value::UInt(u64::MAX)
        );
        assert_eq!(
            to_value(Datum::Dec("12.50".parse().unwrap()), &ft(tp::NEWDECIMAL, 0)).unwrap(),
            Value::Decimal("12.50".into())
        );
        assert_eq!(
            to_value(Datum::Null, &ft(tp::NEWDECIMAL, 0)).unwrap(),
            Value::Null
        );
        assert_eq!(
            to_value(Datum::Bytes(b"ab".to_vec()), &ft(tp::STRING, 0)).unwrap(),
            Value::Bytes(b"ab".to_vec())
        );
        assert_eq!(
            to_value(Datum::F64(1.5), &ft(tp::FLOAT, 0)).unwrap(),
            Value::Float(1.5)
        );
    }
}
