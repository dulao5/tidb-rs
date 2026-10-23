//! Protocol-level column metadata and values, and their text/binary row encodings
//! (TiDB `server/internal/column`, `format/textrow`, `server/internal/dump`).
//!
//! These types are deliberately independent of the executor's datum representation; the SQL
//! layer converts its results into [`Value`]s.

use crate::codec::{put_lenenc_int, put_lenenc_str};
use crate::constants::{collation, column_flag, header};
use crate::error::{SqlError, code};

/// MySQL column/field type byte (`mysql.Type*`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ColumnType {
    Decimal = 0x00,
    Tiny = 0x01,
    Short = 0x02,
    Long = 0x03,
    Float = 0x04,
    Double = 0x05,
    Null = 0x06,
    Timestamp = 0x07,
    LongLong = 0x08,
    Int24 = 0x09,
    Date = 0x0a,
    Duration = 0x0b,
    Datetime = 0x0c,
    Year = 0x0d,
    NewDate = 0x0e,
    Varchar = 0x0f,
    Bit = 0x10,
    Json = 0xf5,
    NewDecimal = 0xf6,
    Enum = 0xf7,
    Set = 0xf8,
    TinyBlob = 0xf9,
    MediumBlob = 0xfa,
    LongBlob = 0xfb,
    Blob = 0xfc,
    VarString = 0xfd,
    String = 0xfe,
    Geometry = 0xff,
}

impl ColumnType {
    pub fn from_u8(b: u8) -> Option<Self> {
        use ColumnType::*;
        Some(match b {
            0x00 => Decimal,
            0x01 => Tiny,
            0x02 => Short,
            0x03 => Long,
            0x04 => Float,
            0x05 => Double,
            0x06 => Null,
            0x07 => Timestamp,
            0x08 => LongLong,
            0x09 => Int24,
            0x0a => Date,
            0x0b => Duration,
            0x0c => Datetime,
            0x0d => Year,
            0x0e => NewDate,
            0x0f => Varchar,
            0x10 => Bit,
            0xf5 => Json,
            0xf6 => NewDecimal,
            0xf7 => Enum,
            0xf8 => Set,
            0xf9 => TinyBlob,
            0xfa => MediumBlob,
            0xfb => LongBlob,
            0xfc => Blob,
            0xfd => VarString,
            0xfe => String,
            0xff => Geometry,
            _ => return None,
        })
    }

    /// String-like types whose column charset is the connection's result charset.
    pub fn is_string(self) -> bool {
        use ColumnType::*;
        matches!(
            self,
            Varchar
                | VarString
                | String
                | TinyBlob
                | MediumBlob
                | LongBlob
                | Blob
                | Enum
                | Set
                | Json
        )
    }

    /// Type byte as sent in a column definition (TiDB `dumpType`).
    fn wire_type(self) -> u8 {
        use ColumnType::*;
        match self {
            Set | Enum => String as u8,
            TinyBlob | MediumBlob | LongBlob => Blob as u8,
            t => t as u8,
        }
    }
}

/// Metadata of one result column (or prepared-statement parameter), i.e. the fields of a
/// `Protocol::ColumnDefinition41` packet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnDef {
    pub schema: String,
    pub table: String,
    pub org_table: String,
    pub name: String,
    pub org_name: String,
    /// Collation id (e.g. [`collation::UTF8MB4_BIN`], [`collation::BINARY`]).
    pub charset: u16,
    /// Display length.
    pub column_length: u32,
    pub column_type: ColumnType,
    /// `column_flag::*` bits.
    pub flags: u16,
    /// Number of fractional digits; also the fsp of temporal columns. 31 = not fixed.
    pub decimals: u8,
}

/// `decimals` value meaning "not fixed" (`mysql.NotFixedDec`).
pub const NOT_FIXED_DEC: u8 = 31;

impl ColumnDef {
    /// A column with defaults suited to `ty`: binary collation and the `BINARY` flag for
    /// non-string types, `utf8mb4_bin` for strings, and a generic display length.
    pub fn new(name: impl Into<String>, ty: ColumnType) -> Self {
        let name = name.into();
        let (charset, flags, len) = if ty.is_string() {
            (collation::UTF8MB4_BIN, 0, 1024)
        } else {
            let len = match ty {
                ColumnType::Tiny => 4,
                ColumnType::Short => 6,
                ColumnType::Int24 => 9,
                ColumnType::Long => 11,
                ColumnType::LongLong => 20,
                ColumnType::Float => 12,
                ColumnType::Double => 22,
                ColumnType::Date => 10,
                ColumnType::Datetime | ColumnType::Timestamp => 19,
                ColumnType::Duration => 10,
                ColumnType::Year => 4,
                _ => 0,
            };
            (collation::BINARY, column_flag::BINARY, len)
        };
        ColumnDef {
            schema: std::string::String::new(),
            table: std::string::String::new(),
            org_table: std::string::String::new(),
            org_name: name.clone(),
            name,
            charset,
            column_length: len,
            column_type: ty,
            flags,
            decimals: 0,
        }
    }

    pub fn with_flags(mut self, flags: u16) -> Self {
        self.flags |= flags;
        self
    }

    pub fn with_length(mut self, len: u32) -> Self {
        self.column_length = len;
        self
    }

    pub fn with_decimals(mut self, decimals: u8) -> Self {
        self.decimals = decimals;
        self
    }

    pub fn is_unsigned(&self) -> bool {
        self.flags & column_flag::UNSIGNED != 0
    }

    /// Encodes a `ColumnDefinition41` payload (TiDB `column.Info.Dump`).
    pub fn encode(&self, buf: &mut Vec<u8>) {
        const MAX_NAME: usize = 256;
        let truncate = |s: &str| -> usize { s.len().min(MAX_NAME) };
        put_lenenc_str(buf, b"def");
        put_lenenc_str(buf, self.schema.as_bytes());
        put_lenenc_str(buf, self.table.as_bytes());
        put_lenenc_str(buf, self.org_table.as_bytes());
        put_lenenc_str(buf, &self.name.as_bytes()[..truncate(&self.name)]);
        put_lenenc_str(buf, &self.org_name.as_bytes()[..truncate(&self.org_name)]);
        buf.push(0x0c);
        buf.extend_from_slice(&self.charset.to_le_bytes());
        buf.extend_from_slice(&self.column_length.to_le_bytes());
        buf.push(self.column_type.wire_type());
        let flags = match self.column_type {
            ColumnType::Set => self.flags | column_flag::SET,
            ColumnType::Enum => self.flags | column_flag::ENUM,
            _ => self.flags,
        };
        buf.extend_from_slice(&flags.to_le_bytes());
        buf.push(self.decimals);
        buf.extend_from_slice(&[0, 0]);
    }
}

/// A calendar date and time of day (DATE, DATETIME, TIMESTAMP). All-zero is MySQL's zero date.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct DateTime {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
    pub microsecond: u32,
}

impl DateTime {
    pub fn date(year: u16, month: u8, day: u8) -> Self {
        DateTime {
            year,
            month,
            day,
            ..Default::default()
        }
    }

    pub fn is_zero(&self) -> bool {
        *self == DateTime::default()
    }
}

/// A TIME value: a signed duration, `hours` may exceed 24 (MySQL range is +-838:59:59).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Duration {
    pub negative: bool,
    pub hours: u32,
    pub minutes: u8,
    pub seconds: u8,
    pub microseconds: u32,
}

impl Duration {
    pub fn is_zero(&self) -> bool {
        self.hours == 0 && self.minutes == 0 && self.seconds == 0 && self.microseconds == 0
    }
}

/// A single cell of a result row, or a decoded statement parameter.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Int(i64),
    UInt(u64),
    Float(f32),
    Double(f64),
    /// A DECIMAL in its canonical text form (e.g. `"-12.340"`).
    Decimal(String),
    /// Strings and binary strings (also JSON/ENUM/SET text, BIT bytes).
    Bytes(Vec<u8>),
    DateTime(DateTime),
    Time(Duration),
}

impl From<&str> for Value {
    fn from(s: &str) -> Self {
        Value::Bytes(s.as_bytes().to_vec())
    }
}

impl From<String> for Value {
    fn from(s: String) -> Self {
        Value::Bytes(s.into_bytes())
    }
}

impl From<i64> for Value {
    fn from(v: i64) -> Self {
        Value::Int(v)
    }
}

impl From<u64> for Value {
    fn from(v: u64) -> Self {
        Value::UInt(v)
    }
}

impl From<f64> for Value {
    fn from(v: f64) -> Self {
        Value::Double(v)
    }
}

fn invalid_type(col: &ColumnDef, v: &Value) -> SqlError {
    SqlError::new(
        code::ER_INVALID_TYPE,
        format!(
            "invalid value {v:?} for column '{}' of type {:?}",
            col.name, col.column_type
        ),
    )
}

fn as_i64(col: &ColumnDef, v: &Value) -> Result<i64, SqlError> {
    match *v {
        Value::Int(i) => Ok(i),
        Value::UInt(u) => Ok(u as i64),
        _ => Err(invalid_type(col, v)),
    }
}

fn as_f64(col: &ColumnDef, v: &Value) -> Result<f64, SqlError> {
    match *v {
        Value::Double(d) => Ok(d),
        Value::Float(f) => Ok(f64::from(f)),
        Value::Int(i) => Ok(i as f64),
        Value::UInt(u) => Ok(u as f64),
        _ => Err(invalid_type(col, v)),
    }
}

/// Appends one text-protocol row (`Protocol::ResultsetRow`): each value as a length-encoded
/// string, NULL as `0xfb`.
pub fn encode_text_row(
    buf: &mut Vec<u8>,
    columns: &[ColumnDef],
    row: &[Value],
) -> Result<(), SqlError> {
    check_width(columns, row)?;
    let mut tmp = Vec::new();
    for (col, v) in columns.iter().zip(row) {
        if matches!(v, Value::Null) {
            buf.push(header::NULL_VALUE);
            continue;
        }
        tmp.clear();
        format_text(&mut tmp, col, v)?;
        put_lenenc_str(buf, &tmp);
    }
    Ok(())
}

fn check_width(columns: &[ColumnDef], row: &[Value]) -> Result<(), SqlError> {
    if columns.len() != row.len() {
        return Err(SqlError::unknown(format!(
            "row has {} values but the result set has {} columns",
            row.len(),
            columns.len()
        )));
    }
    Ok(())
}

/// Text form of a non-NULL value (TiDB `textrow.FormatValueText`).
fn format_text(out: &mut Vec<u8>, col: &ColumnDef, v: &Value) -> Result<(), SqlError> {
    use std::io::Write;
    match v {
        Value::Null => {}
        Value::Int(i) => {
            if col.column_type == ColumnType::Year && *i == 0 {
                out.extend_from_slice(b"0000");
            } else {
                let _ = write!(out, "{i}");
            }
        }
        Value::UInt(u) => {
            let _ = write!(out, "{u}");
        }
        Value::Float(f) => format_float(out, f64::from(*f), float_prec(col), true),
        Value::Double(d) => format_float(out, *d, float_prec(col), false),
        Value::Decimal(s) => out.extend_from_slice(s.as_bytes()),
        Value::Bytes(b) => out.extend_from_slice(b),
        Value::DateTime(t) => {
            let _ = write!(out, "{:04}-{:02}-{:02}", t.year, t.month, t.day);
            if col.column_type != ColumnType::Date && col.column_type != ColumnType::NewDate {
                let _ = write!(out, " {:02}:{:02}:{:02}", t.hour, t.minute, t.second);
                push_fraction(out, t.microsecond, col.decimals);
            }
        }
        Value::Time(d) => {
            if d.negative {
                out.push(b'-');
            }
            let _ = write!(out, "{:02}:{:02}:{:02}", d.hours, d.minutes, d.seconds);
            push_fraction(out, d.microseconds, col.decimals);
        }
    }
    Ok(())
}

fn push_fraction(out: &mut Vec<u8>, micros: u32, fsp: u8) {
    let fsp = usize::from(fsp);
    if fsp == 0 || fsp > 6 {
        return;
    }
    let digits = format!("{micros:06}");
    out.push(b'.');
    out.extend_from_slice(&digits.as_bytes()[..fsp]);
}

/// Fixed precision for float text: only for expression results with explicit decimals.
fn float_prec(col: &ColumnDef) -> Option<usize> {
    if col.decimals > 0 && col.decimals != NOT_FIXED_DEC && col.table.is_empty() {
        Some(usize::from(col.decimals))
    } else {
        None
    }
}

/// MySQL float formatting (TiDB `textrow.AppendFormatFloat`): plain notation, switching to
/// exponent notation without `+` and trailing zeros for |v| >= 1e15 or < 1e-15.
fn format_float(out: &mut Vec<u8>, v: f64, prec: Option<usize>, is_f32: bool) {
    use std::io::Write;
    let abs = v.abs();
    if abs.is_nan() || abs.is_infinite() {
        out.push(b'0');
        return;
    }
    let e_format = if is_f32 {
        let a = abs as f32;
        a >= 1e15 || (a != 0.0 && a < 1e-15)
    } else {
        abs >= 1e15 || (abs != 0.0 && abs < 1e-15)
    };
    if e_format {
        let prec = if is_f32 { Some(5) } else { prec };
        let s = match (prec, is_f32) {
            (Some(p), true) => format!("{:.*e}", p, v as f32),
            (Some(p), false) => format!("{v:.p$e}"),
            (None, true) => format!("{:e}", v as f32),
            (None, false) => format!("{v:e}"),
        };
        // Strip trailing zeros (and a trailing '.') of the mantissa.
        let (mantissa, exp) = s.split_at(s.find('e').unwrap_or(s.len()));
        let mantissa = if mantissa.contains('.') {
            mantissa.trim_end_matches('0').trim_end_matches('.')
        } else {
            mantissa
        };
        out.extend_from_slice(mantissa.as_bytes());
        out.extend_from_slice(exp.as_bytes());
    } else {
        let _ = match (prec, is_f32) {
            (Some(p), true) => write!(out, "{:.*}", p, v as f32),
            (Some(p), false) => write!(out, "{v:.p$}"),
            (None, true) => write!(out, "{}", v as f32),
            (None, false) => write!(out, "{v}"),
        };
    }
}

/// Appends one binary-protocol row (`Protocol::BinaryResultsetRow`): `0x00`, a NULL bitmap
/// with a 2-bit offset, then each non-NULL value encoded according to its column type
/// (TiDB `column.DumpBinaryRow`).
pub fn encode_binary_row(
    buf: &mut Vec<u8>,
    columns: &[ColumnDef],
    row: &[Value],
) -> Result<(), SqlError> {
    check_width(columns, row)?;
    buf.push(header::OK);
    let bitmap_off = buf.len();
    buf.resize(bitmap_off + (columns.len() + 7 + 2) / 8, 0);
    for (i, (col, v)) in columns.iter().zip(row).enumerate() {
        if matches!(v, Value::Null) {
            buf[bitmap_off + (i + 2) / 8] |= 1 << ((i + 2) % 8);
            continue;
        }
        encode_binary_value(buf, col, v)?;
    }
    Ok(())
}

fn encode_binary_value(buf: &mut Vec<u8>, col: &ColumnDef, v: &Value) -> Result<(), SqlError> {
    use ColumnType::*;
    match col.column_type {
        Tiny => buf.push(as_i64(col, v)? as u8),
        Short | Year => buf.extend_from_slice(&(as_i64(col, v)? as u16).to_le_bytes()),
        Int24 | Long => buf.extend_from_slice(&(as_i64(col, v)? as u32).to_le_bytes()),
        LongLong => buf.extend_from_slice(&(as_i64(col, v)? as u64).to_le_bytes()),
        Float => buf.extend_from_slice(&(as_f64(col, v)? as f32).to_bits().to_le_bytes()),
        Double => buf.extend_from_slice(&as_f64(col, v)?.to_bits().to_le_bytes()),
        Decimal | NewDecimal => match v {
            Value::Decimal(s) => put_lenenc_str(buf, s.as_bytes()),
            Value::Int(_) | Value::UInt(_) => {
                let mut tmp = Vec::new();
                format_text(&mut tmp, col, v)?;
                put_lenenc_str(buf, &tmp);
            }
            _ => return Err(invalid_type(col, v)),
        },
        Varchar | VarString | String | Bit | TinyBlob | MediumBlob | LongBlob | Blob | Enum
        | Set | Json | Geometry => match v {
            Value::Bytes(b) => put_lenenc_str(buf, b),
            Value::Decimal(s) => put_lenenc_str(buf, s.as_bytes()),
            // Numbers/temporals in a string column are sent as their text form.
            _ => {
                let mut tmp = Vec::new();
                format_text(&mut tmp, col, v)?;
                put_lenenc_str(buf, &tmp);
            }
        },
        Date | NewDate | Datetime | Timestamp => match v {
            Value::DateTime(t) => encode_binary_datetime(buf, t, col.column_type),
            _ => return Err(invalid_type(col, v)),
        },
        Duration => match v {
            Value::Time(d) => encode_binary_time(buf, d),
            _ => return Err(invalid_type(col, v)),
        },
        Null => return Err(invalid_type(col, v)),
    }
    Ok(())
}

/// Binary DATE/DATETIME/TIMESTAMP (TiDB `dump.BinaryDateTime`): length 0, 4, 7 or 11.
pub fn encode_binary_datetime(buf: &mut Vec<u8>, t: &DateTime, ty: ColumnType) {
    let date_only = matches!(ty, ColumnType::Date | ColumnType::NewDate);
    if t.is_zero() {
        buf.push(0);
        return;
    }
    let len = if date_only {
        4
    } else if t.microsecond != 0 {
        11
    } else if t.hour != 0 || t.minute != 0 || t.second != 0 {
        7
    } else {
        4
    };
    buf.push(len);
    buf.extend_from_slice(&t.year.to_le_bytes());
    buf.extend_from_slice(&[t.month, t.day]);
    if len >= 7 {
        buf.extend_from_slice(&[t.hour, t.minute, t.second]);
    }
    if len == 11 {
        buf.extend_from_slice(&t.microsecond.to_le_bytes());
    }
}

/// Binary TIME (TiDB `dump.BinaryTime`): length 0, 8 or 12; days as a 4-byte integer.
pub fn encode_binary_time(buf: &mut Vec<u8>, d: &Duration) {
    if d.is_zero() {
        buf.push(0);
        return;
    }
    let len = if d.microseconds != 0 { 12 } else { 8 };
    buf.push(len);
    buf.push(u8::from(d.negative));
    buf.extend_from_slice(&(d.hours / 24).to_le_bytes());
    buf.extend_from_slice(&[(d.hours % 24) as u8, d.minutes, d.seconds]);
    if len == 12 {
        buf.extend_from_slice(&d.microseconds.to_le_bytes());
    }
}

/// Length-encoded column count, the first packet of a result set.
pub fn encode_column_count(buf: &mut Vec<u8>, n: usize) {
    put_lenenc_int(buf, n as u64);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(col: &ColumnDef, v: Value) -> String {
        let mut out = Vec::new();
        format_text(&mut out, col, &v).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn column_def_layout() {
        let c = ColumnDef {
            schema: "test".into(),
            table: "t".into(),
            org_table: "t".into(),
            name: "a".into(),
            org_name: "a".into(),
            charset: 63,
            column_length: 11,
            column_type: ColumnType::Long,
            flags: column_flag::NOT_NULL | column_flag::PRI_KEY,
            decimals: 0,
        };
        let mut b = Vec::new();
        c.encode(&mut b);
        let expected: Vec<u8> = [
            &[3][..],
            b"def",
            &[4],
            b"test",
            &[1],
            b"t",
            &[1],
            b"t",
            &[1],
            b"a",
            &[1],
            b"a",
            &[0x0c, 63, 0, 11, 0, 0, 0, 0x03, 0x03, 0x00, 0, 0, 0],
        ]
        .concat();
        assert_eq!(b, expected);
    }

    #[test]
    fn column_def_type_and_flag_mapping() {
        // TiDB dumpType/DumpFlag: ENUM/SET are sent as STRING with ENUM/SET flags, the BLOB
        // family as BLOB.
        let mut b = Vec::new();
        ColumnDef::new("e", ColumnType::Enum).encode(&mut b);
        let n = b.len();
        assert_eq!(b[n - 6], ColumnType::String as u8);
        assert_eq!(u16::from_le_bytes([b[n - 5], b[n - 4]]), column_flag::ENUM);
        b.clear();
        ColumnDef::new("x", ColumnType::LongBlob).encode(&mut b);
        assert_eq!(b[b.len() - 6], ColumnType::Blob as u8);
    }

    #[test]
    fn text_formatting() {
        let int = ColumnDef::new("a", ColumnType::LongLong);
        assert_eq!(text(&int, Value::Int(-5)), "-5");
        assert_eq!(text(&int, Value::UInt(u64::MAX)), "18446744073709551615");
        let year = ColumnDef::new("y", ColumnType::Year);
        assert_eq!(text(&year, Value::Int(0)), "0000");
        let dbl = ColumnDef::new("d", ColumnType::Double);
        assert_eq!(text(&dbl, Value::Double(1.5)), "1.5");
        assert_eq!(text(&dbl, Value::Double(0.1)), "0.1");
        assert_eq!(text(&dbl, Value::Double(1e15)), "1e15");
        assert_eq!(text(&dbl, Value::Double(1.25e-16)), "1.25e-16");
        assert_eq!(
            text(&dbl, Value::Double(123456789012345.0)),
            "123456789012345"
        );
        assert_eq!(text(&dbl, Value::Double(f64::NAN)), "0");
        let dbl2 = ColumnDef::new("d", ColumnType::Double).with_decimals(2);
        assert_eq!(text(&dbl2, Value::Double(1.0)), "1.00");
        let flt = ColumnDef::new("f", ColumnType::Float);
        assert_eq!(text(&flt, Value::Float(0.1)), "0.1");
        assert_eq!(text(&flt, Value::Float(1.5e20)), "1.5e20");
        let dt = ColumnDef::new("t", ColumnType::Datetime).with_decimals(3);
        let v = DateTime {
            year: 2010,
            month: 10,
            day: 17,
            hour: 19,
            minute: 27,
            second: 30,
            microsecond: 1234,
        };
        assert_eq!(text(&dt, Value::DateTime(v)), "2010-10-17 19:27:30.001");
        let date = ColumnDef::new("d", ColumnType::Date);
        assert_eq!(text(&date, Value::DateTime(v)), "2010-10-17");
        let tm = ColumnDef::new("t", ColumnType::Duration);
        let d = Duration {
            negative: true,
            hours: 838,
            minutes: 59,
            seconds: 59,
            microseconds: 0,
        };
        assert_eq!(text(&tm, Value::Time(d)), "-838:59:59");
    }

    #[test]
    fn text_row_with_null() {
        let cols = [
            ColumnDef::new("a", ColumnType::LongLong),
            ColumnDef::new("b", ColumnType::VarString),
        ];
        let mut b = Vec::new();
        encode_text_row(&mut b, &cols, &[Value::Int(12), Value::Null]).unwrap();
        assert_eq!(b, [2, b'1', b'2', 0xfb]);
        assert!(encode_text_row(&mut b, &cols, &[Value::Null]).is_err());
    }

    #[test]
    fn binary_row_layout() {
        let cols = [
            ColumnDef::new("tiny", ColumnType::Tiny),
            ColumnDef::new("null", ColumnType::LongLong),
            ColumnDef::new("short", ColumnType::Short),
            ColumnDef::new("long", ColumnType::Long),
            ColumnDef::new("ll", ColumnType::LongLong),
            ColumnDef::new("dbl", ColumnType::Double),
            ColumnDef::new("flt", ColumnType::Float),
            ColumnDef::new("dec", ColumnType::NewDecimal),
            ColumnDef::new("s", ColumnType::VarString),
        ];
        let row = [
            Value::Int(-1),
            Value::Null,
            Value::Int(0x1234),
            Value::Int(-2),
            Value::UInt(u64::MAX),
            Value::Double(1.5),
            Value::Float(0.5),
            Value::Decimal("1.20".into()),
            Value::from("ab"),
        ];
        let mut b = Vec::new();
        encode_binary_row(&mut b, &cols, &row).unwrap();
        let mut expected = vec![0x00];
        // 9 columns -> (9 + 7 + 2) / 8 = 2 bitmap bytes; column 1 -> bit 3 of byte 0.
        expected.extend_from_slice(&[0b0000_1000, 0]);
        expected.push(0xff);
        expected.extend_from_slice(&[0x34, 0x12]);
        expected.extend_from_slice(&[0xfe, 0xff, 0xff, 0xff]);
        expected.extend_from_slice(&[0xff; 8]);
        expected.extend_from_slice(&1.5f64.to_bits().to_le_bytes());
        expected.extend_from_slice(&0.5f32.to_bits().to_le_bytes());
        expected.extend_from_slice(b"\x041.20");
        expected.extend_from_slice(b"\x02ab");
        assert_eq!(b, expected);
    }

    #[test]
    fn binary_null_bitmap_offsets() {
        // The 2-bit offset spills into a new byte at column 6 (TiDB numBytes4Null).
        let cols: Vec<_> = (0..7)
            .map(|i| ColumnDef::new(format!("c{i}"), ColumnType::LongLong))
            .collect();
        let mut b = Vec::new();
        encode_binary_row(&mut b, &cols, &vec![Value::Null; 7]).unwrap();
        assert_eq!(b, [0x00, 0b1111_1100, 0b0000_0001]);
    }

    #[test]
    fn binary_datetime_and_time_match_tidb_dump() {
        // Vectors from TiDB's dump_test.go TestDumpBinaryTime.
        let enc_dt = |t: DateTime, ty| {
            let mut b = Vec::new();
            encode_binary_datetime(&mut b, &t, ty);
            b
        };
        let enc_tm = |d: Duration| {
            let mut b = Vec::new();
            encode_binary_time(&mut b, &d);
            b
        };
        assert_eq!(enc_dt(DateTime::default(), ColumnType::Timestamp), [0]);
        let t = DateTime {
            year: 1991,
            month: 5,
            day: 1,
            hour: 1,
            minute: 1,
            second: 1,
            microsecond: 100_001,
        };
        assert_eq!(
            enc_dt(t, ColumnType::Timestamp),
            [11, 199, 7, 5, 1, 1, 1, 1, 161, 134, 1, 0]
        );
        let t = DateTime {
            year: 1993,
            month: 7,
            day: 13,
            hour: 1,
            minute: 1,
            second: 1,
            microsecond: 0,
        };
        assert_eq!(enc_dt(t, ColumnType::Datetime), [7, 201, 7, 7, 13, 1, 1, 1]);
        assert_eq!(
            enc_dt(DateTime::date(1992, 6, 1), ColumnType::Date),
            [4, 200, 7, 6, 1]
        );
        assert_eq!(
            enc_dt(DateTime::date(1992, 6, 1), ColumnType::Datetime),
            [4, 200, 7, 6, 1]
        );
        assert_eq!(enc_dt(DateTime::default(), ColumnType::Date), [0]);
        assert_eq!(enc_tm(Duration::default()), [0]);
        // BinaryTime(-1ns): negative, sub-microsecond -> 12 bytes, zero micros in TiDB too.
        let d = Duration {
            negative: true,
            hours: 0,
            minutes: 0,
            seconds: 0,
            microseconds: 0,
        };
        assert_eq!(enc_tm(d), [0]); // TiDB keeps the sign for -1ns; a zero TIME has no sign here.
        // BinaryTime(86.4s + 1ns)
        let d = Duration {
            negative: false,
            hours: 0,
            minutes: 1,
            seconds: 26,
            microseconds: 400_000,
        };
        assert_eq!(enc_tm(d), [12, 0, 0, 0, 0, 0, 0, 1, 26, 128, 26, 6, 0]);
        // Days are a 4-byte little-endian integer.
        let d = Duration {
            negative: true,
            hours: 26,
            minutes: 2,
            seconds: 3,
            microseconds: 0,
        };
        assert_eq!(enc_tm(d), [8, 1, 1, 0, 0, 0, 2, 2, 3]);
    }

    #[test]
    fn binary_type_mismatch_is_an_error() {
        let cols = [ColumnDef::new("a", ColumnType::LongLong)];
        let mut b = Vec::new();
        assert!(encode_binary_row(&mut b, &cols, &[Value::from("x")]).is_err());
    }
}
