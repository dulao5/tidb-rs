//! Decoding of `COM_STMT_EXECUTE` parameters (TiDB `server/conn_stmt_params.go`).

use crate::codec::Reader;
use crate::error::{SqlError, code};
use crate::value::{ColumnType, DateTime, Duration, Value};

/// A bound statement parameter: the wire type the client declared plus the decoded value.
#[derive(Clone, Debug, PartialEq)]
pub struct Param {
    pub ty: ColumnType,
    /// The client set the unsigned bit for this parameter's type.
    pub unsigned: bool,
    pub value: Value,
}

impl Param {
    pub fn null() -> Self {
        Param {
            ty: ColumnType::Null,
            unsigned: false,
            value: Value::Null,
        }
    }
}

/// Decodes `count` parameters.
///
/// * `long_data[i]` holds data sent with `COM_STMT_SEND_LONG_DATA`, which takes precedence over
///   everything else (as in TiDB, some clients also set the NULL bit for such params).
/// * `null_bitmap` has `(count + 7) / 8` bytes.
/// * `types` holds two bytes (type, flags) per parameter, from this or an earlier execute.
pub fn decode_params(
    count: usize,
    long_data: &[Option<Vec<u8>>],
    null_bitmap: &[u8],
    types: &[u8],
    values: &[u8],
) -> Result<Vec<Param>, SqlError> {
    let mut r = Reader::new(values);
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        if let Some(Some(data)) = long_data.get(i) {
            let ty = types
                .get(2 * i)
                .and_then(|&t| ColumnType::from_u8(t))
                .filter(|t| t.is_string() || *t == ColumnType::Bit)
                .unwrap_or(ColumnType::Blob);
            out.push(Param {
                ty,
                unsigned: false,
                value: Value::Bytes(data.clone()),
            });
            continue;
        }
        if null_bitmap.get(i / 8).ok_or_else(SqlError::malformed)? & (1 << (i % 8)) != 0 {
            out.push(Param::null());
            continue;
        }
        let (&tp, &flags) = match (types.get(2 * i), types.get(2 * i + 1)) {
            (Some(t), Some(f)) => (t, f),
            _ => return Err(SqlError::malformed()),
        };
        let unsigned = flags & 0x80 != 0;
        let ty = ColumnType::from_u8(tp).ok_or_else(|| {
            SqlError::new(
                code::ER_UNKNOWN_FIELD_TYPE,
                format!("stmt unknown field type {tp}"),
            )
        })?;
        let value = decode_value(&mut r, ty, unsigned)?;
        out.push(Param {
            ty,
            unsigned,
            value,
        });
    }
    Ok(out)
}

fn decode_value(r: &mut Reader<'_>, ty: ColumnType, unsigned: bool) -> Result<Value, SqlError> {
    use ColumnType::*;
    let int = |v: i64, u: u64| {
        if unsigned {
            Value::UInt(u)
        } else {
            Value::Int(v)
        }
    };
    Ok(match ty {
        Null => Value::Null,
        Tiny => {
            let b = r.u8()?;
            int(i64::from(b as i8), u64::from(b))
        }
        Short | Year => {
            let v = r.u16()?;
            int(i64::from(v as i16), u64::from(v))
        }
        Int24 | Long => {
            let v = r.u32()?;
            int(i64::from(v as i32), u64::from(v))
        }
        LongLong => {
            let v = r.u64()?;
            int(v as i64, v)
        }
        Float => Value::Float(f32::from_bits(r.u32()?)),
        Double => Value::Double(f64::from_bits(r.u64()?)),
        Date | Timestamp | Datetime | NewDate => {
            let len = usize::from(r.u8()?);
            Value::DateTime(decode_datetime(r.take(len)?)?)
        }
        Duration => {
            let len = usize::from(r.u8()?);
            Value::Time(decode_duration(r.take(len)?)?)
        }
        Decimal | NewDecimal => match r.lenenc_str()? {
            None => Value::Null,
            Some(s) => Value::Decimal(
                std::string::String::from_utf8(s.to_vec()).map_err(|_| SqlError::malformed())?,
            ),
        },
        Varchar | VarString | String | Enum | Set | Geometry | Bit | TinyBlob | MediumBlob
        | LongBlob | Blob | Json => match r.lenenc_str()? {
            None => Value::Null,
            Some(s) => Value::Bytes(s.to_vec()),
        },
    })
}

/// Binary DATE/DATETIME/TIMESTAMP parameter body (length 0, 4, 7 or 11).
fn decode_datetime(b: &[u8]) -> Result<DateTime, SqlError> {
    let mut r = Reader::new(b);
    let mut t = DateTime::default();
    match b.len() {
        0 => {}
        4 | 7 | 11 => {
            t.year = r.u16()?;
            t.month = r.u8()?;
            t.day = r.u8()?;
            if b.len() >= 7 {
                t.hour = r.u8()?;
                t.minute = r.u8()?;
                t.second = r.u8()?;
            }
            if b.len() == 11 {
                t.microsecond = r.u32()?;
            }
        }
        // 13 bytes (with a time-zone offset, MySQL 8.0.19+) is not supported yet.
        _ => return Err(SqlError::malformed()),
    }
    Ok(t)
}

/// Binary TIME parameter body (length 0, 8 or 12).
fn decode_duration(b: &[u8]) -> Result<Duration, SqlError> {
    let mut r = Reader::new(b);
    let mut d = Duration::default();
    match b.len() {
        0 => {}
        8 | 12 => {
            d.negative = r.u8()? == 1;
            let days = r.u32()?;
            let hours = r.u8()?;
            d.hours = days.saturating_mul(24).saturating_add(u32::from(hours));
            d.minutes = r.u8()?;
            d.seconds = r.u8()?;
            if b.len() == 12 {
                d.microseconds = r.u32()?;
            }
        }
        _ => return Err(SqlError::malformed()),
    }
    Ok(d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(tp: u8, values: &[u8]) -> Result<Param, SqlError> {
        decode_params(1, &[None], &[0], &[tp, 0], values).map(|mut v| v.remove(0))
    }

    #[test]
    fn integers_sign_extend() {
        // TiDB TestParseExecArgs "int overflow" cases.
        assert_eq!(one(1, &[0xff]).unwrap().value, Value::Int(-1));
        assert_eq!(one(2, &[0xff, 0xff]).unwrap().value, Value::Int(-1));
        assert_eq!(
            one(3, &[0xff, 0xff, 0xff, 0xff]).unwrap().value,
            Value::Int(-1)
        );
        assert_eq!(one(8, &[0xff; 8]).unwrap().value, Value::Int(-1));
        let p = decode_params(1, &[None], &[0], &[1, 0x80], &[0xff]).unwrap();
        assert_eq!(p[0].value, Value::UInt(255));
        assert!(p[0].unsigned);
    }

    #[test]
    fn datetimes() {
        // TiDB TestParseExecArgs date/datetime/timestamp cases.
        let v = one(
            12,
            &[
                0x0b, 0xda, 0x07, 0x0a, 0x11, 0x13, 0x1b, 0x1e, 0x01, 0x00, 0x00, 0x00,
            ],
        )
        .unwrap();
        assert_eq!(
            v.value,
            Value::DateTime(DateTime {
                year: 2010,
                month: 10,
                day: 17,
                hour: 19,
                minute: 27,
                second: 30,
                microsecond: 1
            })
        );
        let v = one(10, &[0x04, 0xda, 0x07, 0x0a, 0x11]).unwrap();
        assert_eq!(v.ty, ColumnType::Date);
        assert_eq!(v.value, Value::DateTime(DateTime::date(2010, 10, 17)));
        let v = one(7, &[0x07, 0xda, 0x07, 0x0a, 0x11, 0x13, 0x1b, 0x1e]).unwrap();
        assert_eq!(
            v.value,
            Value::DateTime(DateTime {
                year: 2010,
                month: 10,
                day: 17,
                hour: 19,
                minute: 27,
                second: 30,
                microsecond: 0
            })
        );
        assert_eq!(
            one(7, &[0x00]).unwrap().value,
            Value::DateTime(DateTime::default())
        );
        // Truncated body.
        assert!(one(7, &[10]).is_err());
    }

    #[test]
    fn durations() {
        // TiDB TestParseExecArgs time cases (range checking is left to the SQL layer).
        let v = one(
            11,
            &[
                0x0c, 0x01, 0x78, 0x00, 0x00, 0x00, 0x13, 0x1b, 0x1e, 0x01, 0x00, 0x00, 0x00,
            ],
        )
        .unwrap();
        assert_eq!(
            v.value,
            Value::Time(Duration {
                negative: true,
                hours: 120 * 24 + 19,
                minutes: 27,
                seconds: 30,
                microseconds: 1
            })
        );
        let v = one(11, &[0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x02, 0x03]).unwrap();
        assert_eq!(
            v.value,
            Value::Time(Duration {
                negative: false,
                hours: 1,
                minutes: 2,
                seconds: 3,
                microseconds: 0
            })
        );
        assert_eq!(
            one(11, &[0x00]).unwrap().value,
            Value::Time(Duration::default())
        );
        assert!(one(11, &[10]).is_err());
        assert!(one(11, &[8, 2]).is_err());
    }

    #[test]
    fn strings_doubles_decimals() {
        assert_eq!(one(0xfd, b"\x03abc").unwrap().value, Value::from("abc"));
        assert_eq!(
            one(0xfc, b"\x02\x00\x01").unwrap().value,
            Value::Bytes(vec![0, 1])
        );
        assert_eq!(
            one(5, &2.5f64.to_le_bytes()).unwrap().value,
            Value::Double(2.5)
        );
        assert_eq!(
            one(4, &1.5f32.to_le_bytes()).unwrap().value,
            Value::Float(1.5)
        );
        assert_eq!(
            one(0xf6, b"\x041.25").unwrap().value,
            Value::Decimal("1.25".into())
        );
        assert_eq!(one(6, &[]).unwrap().value, Value::Null);
    }

    #[test]
    fn malformed_lenenc_does_not_panic() {
        // TiDB TestParseExecArgsMalformedLengthEncodedParam.
        assert!(one(0xfd, &[0xfe]).is_err());
        assert!(one(0xfd, &[0xfe, 0, 0, 0, 0, 0, 0, 0, 0x80]).is_err());
        assert!(one(8, &[1, 2, 3]).is_err());
        // Missing type information.
        assert!(decode_params(1, &[None], &[0], &[], &[1]).is_err());
        // Unknown type byte.
        assert_eq!(
            one(0x40, &[1]).unwrap_err().code,
            code::ER_UNKNOWN_FIELD_TYPE
        );
    }

    #[test]
    fn null_bitmap_and_long_data() {
        // Three params: NULL, long data (NULL bit set too, as MariaDB clients do), int.
        let long = [None, Some(b"long".to_vec()), None];
        let p = decode_params(
            3,
            &long,
            &[0b011],
            &[8, 0, 0xfe, 0, 8, 0],
            &7i64.to_le_bytes(),
        )
        .unwrap();
        assert_eq!(p[0], Param::null());
        assert_eq!(
            p[1],
            Param {
                ty: ColumnType::String,
                unsigned: false,
                value: Value::from("long")
            }
        );
        assert_eq!(p[2].value, Value::Int(7));
        // Long data without type info defaults to BLOB.
        let p = decode_params(1, &[Some(vec![1])], &[0], &[], &[]).unwrap();
        assert_eq!(p[0].ty, ColumnType::Blob);
    }
}
