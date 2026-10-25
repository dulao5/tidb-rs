//! Result-set column metadata, ported from Go TiDB's `server/internal/column.ConvertColumnInfo`
//! and `executor.colNames2ResultFields`.

use tidb_meta::model::FieldType;
use tidb_meta::model::tp;
use tidb_mysql_protocol::ColumnDef;
use tidb_mysql_protocol::ColumnType;

/// `types.UnspecifiedLength`.
pub const UNSPECIFIED_LENGTH: i64 = -1;

/// Names of one result column, as Go's `types.FieldName` turned into a `ResultField`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FieldNames {
    /// Database (`ResultField.DBName`).
    pub schema: String,
    /// Table alias or name (`TableAsName`).
    pub table: String,
    /// Real table name (`Table.Name`).
    pub org_table: String,
    /// Column alias or name as written (`ColumnAsName`).
    pub name: String,
    /// Real column name; empty for expressions (`EmptyOrgName`).
    pub org_name: String,
}

/// Collation id of a charset's default collation (Go: `mysql.CharsetNameToID`).
pub fn charset_name_to_id(charset: &str) -> u16 {
    match charset {
        "utf8mb4" => 46,
        "binary" => 63,
        "utf8" => 83,
        "ascii" => 65,
        "latin1" => 47,
        "gbk" => 28,
        "gb18030" => 248,
        "big5" => 1,
        _ => 0,
    }
}

/// Maximum bytes per character (Go: `charset.GetCharsetInfo(..).Maxlen`); `None` if unknown.
fn charset_maxlen(charset: &str) -> Option<u32> {
    Some(match charset {
        "utf8mb4" | "gb18030" => 4,
        "utf8" => 3,
        "gbk" => 2,
        "ascii" | "latin1" | "binary" => 1,
        _ => return None,
    })
}

/// Go `mysql.GetDefaultFieldLengthAndDecimal(tp).flen`.
fn default_flen(t: u8) -> i64 {
    match t {
        tp::BIT => 1,
        tp::TINY => 4,
        tp::SHORT => 6,
        tp::INT24 => 9,
        tp::LONG => 11,
        tp::LONGLONG => 20,
        tp::DOUBLE => 22,
        tp::FLOAT => 12,
        tp::NEWDECIMAL => 10,
        tp::DURATION | tp::DATE => 10,
        tp::TIMESTAMP | tp::DATETIME => 19,
        tp::YEAR => 4,
        tp::STRING => 1,
        tp::VARCHAR | tp::VAR_STRING => 5,
        tp::TINY_BLOB => 255,
        tp::BLOB => 65535,
        tp::MEDIUM_BLOB => 16_777_215,
        tp::LONG_BLOB | tp::JSON => 4_294_967_295,
        tp::NULL => 0,
        _ => -1,
    }
}

/// Go `types.IsString`.
fn is_string(t: u8) -> bool {
    matches!(
        t,
        tp::STRING
            | tp::VARCHAR
            | tp::VAR_STRING
            | tp::TINY_BLOB
            | tp::MEDIUM_BLOB
            | tp::LONG_BLOB
            | tp::BLOB
    )
}

/// The protocol column definition of a result column of type `ft` (Go: `ConvertColumnInfo`).
pub fn column_def(ft: &FieldType, names: &FieldNames) -> ColumnDef {
    let mut t = ft.tp;
    let mut len: u32;
    if ft.flen != UNSPECIFIED_LENGTH {
        // Go: uint32(flen), wrapping like the conversion and multiplication there.
        len = ft.flen as u32;
        if t == tp::NEWDECIMAL {
            len = len.wrapping_add(1);
            if ft.decimal > 0 {
                len = len.wrapping_add(1);
            }
        } else if is_string(t) || t == tp::ENUM || t == tp::SET {
            len = len.wrapping_mul(charset_maxlen(&ft.charset).unwrap_or(4));
        }
    } else {
        len = default_flen(t) as u32;
    }
    let decimals = if ft.decimal == UNSPECIFIED_LENGTH {
        if t == tp::DURATION { 0 } else { 31 }
    } else {
        ft.decimal as u8
    };
    if t == tp::VARCHAR {
        t = tp::VAR_STRING;
    }
    ColumnDef {
        schema: names.schema.clone(),
        table: names.table.clone(),
        org_table: names.org_table.clone(),
        name: names.name.clone(),
        org_name: names.org_name.clone(),
        charset: charset_name_to_id(&ft.charset),
        column_length: len,
        column_type: ColumnType::from_u8(t).unwrap_or(ColumnType::VarString),
        flags: ft.flag as u16,
        decimals,
    }
}

/// Go `ResultEncoder.ColumnCharsetID` with `@@character_set_results`: string columns that are
/// not binary are advertised in the result charset.
pub fn apply_result_charset(col: &mut ColumnDef, results_charset: Option<&str>) {
    let Some(cs) = results_charset else { return };
    if cs.is_empty() || !is_string_column_type(col.column_type) || col.charset == 63 {
        return;
    }
    col.charset = charset_name_to_id(cs);
}

/// Go `textrow.IsStringColumnType`.
fn is_string_column_type(t: ColumnType) -> bool {
    use ColumnType::*;
    matches!(
        t,
        String
            | VarString
            | Varchar
            | Bit
            | TinyBlob
            | MediumBlob
            | LongBlob
            | Blob
            | Enum
            | Set
            | Json
    )
}

#[cfg(test)]
mod tests {
    use tidb_meta::model::flag;

    use super::*;

    fn ft(t: u8, flag: u32, flen: i64, decimal: i64, charset: &str) -> FieldType {
        FieldType {
            tp: t,
            flag,
            flen,
            decimal,
            charset: charset.to_owned(),
            collate: if charset == "binary" {
                "binary".to_owned()
            } else {
                "utf8mb4_bin".to_owned()
            },
            ..FieldType::default()
        }
    }

    #[test]
    fn sysbench_columns() {
        let names = FieldNames {
            schema: "sbtest".into(),
            table: "sbtest1".into(),
            org_table: "sbtest1".into(),
            name: "c".into(),
            org_name: "c".into(),
        };
        // c CHAR(120) NOT NULL, utf8mb4: length 480, STRING, collation 46.
        let c = column_def(&ft(tp::STRING, flag::NOT_NULL, 120, 0, "utf8mb4"), &names);
        assert_eq!(c.column_type, ColumnType::String);
        assert_eq!(c.column_length, 480);
        assert_eq!(c.charset, 46);
        assert_eq!(c.flags, 1);
        assert_eq!(c.decimals, 0);
        // id INT NOT NULL PRIMARY KEY AUTO_INCREMENT: LONG(11), binary.
        let id = column_def(
            &ft(
                tp::LONG,
                flag::NOT_NULL | flag::PRI_KEY | flag::AUTO_INCREMENT,
                11,
                0,
                "binary",
            ),
            &names,
        );
        assert_eq!(id.column_type, ColumnType::Long);
        assert_eq!(id.column_length, 11);
        assert_eq!(id.charset, 63);
        assert_eq!(id.flags, 1 | 2 | 512);
        // SUM(INT): DECIMAL(32,0) -> length 33 (sign).
        let sum = column_def(
            &ft(tp::NEWDECIMAL, flag::BINARY, 32, 0, "binary"),
            &FieldNames::default(),
        );
        assert_eq!(sum.column_type, ColumnType::NewDecimal);
        assert_eq!(sum.column_length, 33);
        assert_eq!(sum.decimals, 0);
        assert_eq!(sum.flags, 128);
        // DECIMAL(10,2): 10 + sign + point.
        let d = column_def(
            &ft(tp::NEWDECIMAL, 0, 10, 2, "binary"),
            &FieldNames::default(),
        );
        assert_eq!(d.column_length, 12);
        assert_eq!(d.decimals, 2);
    }

    #[test]
    fn varchar_unspecified_and_wrapping() {
        let v = column_def(&ft(tp::VARCHAR, 0, 10, 0, "utf8"), &FieldNames::default());
        assert_eq!(v.column_type, ColumnType::VarString);
        assert_eq!(v.column_length, 30);
        let dbl = column_def(
            &ft(tp::DOUBLE, flag::BINARY, -1, -1, "binary"),
            &FieldNames::default(),
        );
        assert_eq!(dbl.column_length, 22);
        assert_eq!(dbl.decimals, 31);
        let dur = column_def(
            &ft(tp::DURATION, flag::BINARY, -1, -1, "binary"),
            &FieldNames::default(),
        );
        assert_eq!(dur.decimals, 0);
        // LONGTEXT utf8mb4: 4294967295 * 4 wraps in uint32 as in Go.
        let lt = column_def(
            &ft(tp::LONG_BLOB, 0, 4_294_967_295, 0, "utf8mb4"),
            &FieldNames::default(),
        );
        assert_eq!(lt.column_length, 4_294_967_295u32.wrapping_mul(4));
        assert_eq!(lt.column_type, ColumnType::LongBlob);
    }

    #[test]
    fn result_charset_override() {
        let mut c = column_def(&ft(tp::STRING, 0, 10, 0, "utf8mb4"), &FieldNames::default());
        apply_result_charset(&mut c, Some("utf8"));
        assert_eq!(c.charset, 83);
        let mut n = column_def(&ft(tp::LONG, 0, 11, 0, "binary"), &FieldNames::default());
        apply_result_charset(&mut n, Some("utf8"));
        assert_eq!(n.charset, 63);
    }
}
