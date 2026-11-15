//! go-file: pkg/server/internal/column/convert.go
//!
//! The column definition of a result field.

#![allow(non_upper_case_globals)]

use tidb_base::mysql::*;
use tidb_executor::glue::ResultField;
use tidb_parser::gocharset::get_charset_info;
use tidb_types::field_type::UnspecifiedLength;

use crate::column::Info;

/// ConvertColumnInfo converts `*ast.ResultField` to `*Info`
///
/// Go reads `fld.Column` (a `ColumnInfo`); the Rust `ResultField` carries its `FieldType`
/// (`column_type`) and name, which is everything read here except the default value.
// go: ConvertColumnInfo L2 (DefaultValue: ResultField has no ColumnInfo, only COM_FIELD_LIST sends it)
pub fn convert_column_info(fld: &ResultField) -> Info {
    let col = &fld.column_type;
    let mut ci = Info {
        name: fld.column_as_name.o.clone(),
        org_name: fld.column_name.o.clone(),
        table: fld.table_as_name.o.clone(),
        schema: fld.db_name.o.clone(),
        flag: col.get_flag() as u16,
        charset: u16::from(charset_name_to_id(col.get_charset())),
        tp: col.get_type(),
        default_value: None,
        ..Info::default()
    };

    if fld.empty_org_name {
        ci.org_name = String::new();
    }
    // `if fld.Table != nil`: the Rust field has the table's name, empty without a table.
    ci.org_table = fld.table_name.o.clone();
    if col.get_flen() != UnspecifiedLength {
        ci.column_length = col.get_flen() as u32;
        if col.get_type() == TypeNewDecimal {
            // Consider the negative sign.
            ci.column_length = ci.column_length.wrapping_add(1);
            if col.get_decimal() > tidb_types::fsp::DefaultFsp {
                // Consider the decimal point.
                ci.column_length = ci.column_length.wrapping_add(1);
            }
        } else if tidb_types::etc::is_string(col.get_type())
            || col.get_type() == TypeEnum
            || col.get_type() == TypeSet
        {
            // issue `18870`, `4540`: the flen is a hint; some clients (old Navicat) truncate
            // `SHOW CREATE TABLE` with a byte length, so it is multiplied by the charset's
            // maximum bytes per character.
            let (charset_desc, err) = get_charset_info(col.get_charset());
            match (charset_desc, err) {
                (Some(desc), Ok(())) => {
                    ci.column_length = ci.column_length.wrapping_mul(desc.maxlen as u32)
                }
                _ => ci.column_length = ci.column_length.wrapping_mul(4),
            }
        }
    } else {
        // issue `60503`: MySQL will always output a usable length; while we cannot output the
        // same length, we can give a default flen. It is not placed before the two branches
        // above because the default flen is already large enough.
        let (clen, _) = get_default_field_length_and_decimal(col.get_type());
        ci.column_length = clen as u32;
    }

    if col.get_decimal() == UnspecifiedLength {
        if col.get_type() == TypeDuration {
            ci.decimal = tidb_types::fsp::DefaultFsp as u8;
        } else {
            ci.decimal = NotFixedDec as u8;
        }
    } else {
        ci.decimal = col.get_decimal() as u8;
    }

    // Keep things compatible for old clients.
    // Refer to mysql-server/sql/protocol.cc send_result_set_metadata()
    if ci.tp == TypeVarchar {
        ci.tp = TypeVarString;
    }
    ci
}
