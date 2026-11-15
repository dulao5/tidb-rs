//! go-file: pkg/server/internal/column/column.go
//!
//! Column definitions (`ColumnDefinition41`) and the text / binary row encodings of a result
//! set.

#![allow(non_upper_case_globals)]

use std::sync::LazyLock;

use tidb_base::Error;
use tidb_base::dbterror::ClassServer;
use tidb_base::errno;
use tidb_base::gofmt::Arg;
use tidb_base::mysql::*;
use tidb_chunk::row::Row;
use tidb_parser::gocharset::CharsetUTF8MB4;
use tidb_types::field_type::UnspecifiedLength;

pub use crate::convert::convert_column_info;
use crate::dump;
pub use crate::result_encoder::ResultEncoder;
use crate::util::append_format_float;

const MAX_COLUMN_NAME_SIZE: usize = 256;

/// `server/err.ErrInvalidType`.
pub static ErrInvalidType: LazyLock<Error> =
    LazyLock::new(|| ClassServer.new_std(errno::ErrInvalidType));

/// Info contains information of a column
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Info {
    /// `DefaultValue any`, as Go prints it with `%v` (`None`: nil). Only `COM_FIELD_LIST`
    /// sends it; [`convert_column_info`] cannot fill it (the Rust `ResultField` has the
    /// column's type, not its `ColumnInfo`), see spec D03.1 §4.
    pub default_value: Option<String>,
    pub schema: String,
    pub table: String,
    pub org_table: String,
    pub name: String,
    pub org_name: String,
    pub column_length: u32,
    pub charset: u16,
    pub flag: u16,
    pub decimal: u8,
    pub tp: u8,
}

impl Info {
    /// Dump dumps Info to bytes.
    // go: Info.Dump L1
    pub fn dump(&self, buffer: &mut Vec<u8>, d: Option<&mut ResultEncoder>) {
        self.dump_inner(buffer, d, false)
    }

    /// DumpWithDefault dumps Info to bytes, including column defaults. This is used for ComFieldList responses.
    // go: Info.DumpWithDefault L1
    pub fn dump_with_default(&self, buffer: &mut Vec<u8>, d: Option<&mut ResultEncoder>) {
        self.dump_inner(buffer, d, true)
    }

    // go: Info.dump L1
    fn dump_inner(&self, buffer: &mut Vec<u8>, d: Option<&mut ResultEncoder>, with_default: bool) {
        let mut own;
        let d = match d {
            Some(d) => d,
            None => {
                own = ResultEncoder::new(CharsetUTF8MB4);
                &mut own
            }
        };
        let mut name_dump = self.name.as_bytes();
        let mut orgname_dump = self.org_name.as_bytes();
        if name_dump.len() > MAX_COLUMN_NAME_SIZE {
            name_dump = &name_dump[0..MAX_COLUMN_NAME_SIZE];
        }
        if orgname_dump.len() > MAX_COLUMN_NAME_SIZE {
            orgname_dump = &orgname_dump[0..MAX_COLUMN_NAME_SIZE];
        }
        dump::length_encoded_string(buffer, b"def");
        dump::length_encoded_string(buffer, &d.encode_meta(self.schema.as_bytes()));
        dump::length_encoded_string(buffer, &d.encode_meta(self.table.as_bytes()));
        dump::length_encoded_string(buffer, &d.encode_meta(self.org_table.as_bytes()));
        dump::length_encoded_string(buffer, &d.encode_meta(name_dump));
        dump::length_encoded_string(buffer, &d.encode_meta(orgname_dump));

        buffer.push(0x0c);
        dump::uint16(buffer, d.column_type_info_charset_id(self));
        dump::uint32(buffer, self.dump_length());
        buffer.push(dump_type(self.tp));
        dump::uint16(buffer, dump_flag(self.tp, self.flag));
        buffer.push(self.decimal);
        buffer.extend_from_slice(&[0, 0]);

        if with_default {
            match self.default_value.as_deref() {
                Some("CURRENT_TIMESTAMP") | Some("CURRENT_DATE") | None => buffer.push(251), // NULL
                Some(v) => dump::length_encoded_string(buffer, v.as_bytes()),
            }
        }
    }

    // go: Info.dumpCharset L1
    pub(crate) fn dump_charset(&self) -> u16 {
        match self.tp {
            // When passing Vector column to the SQL Client, pretend to be a non-binary String.
            TypeTiDBVectorFloat32 => DefaultCollationID as u16,
            _ => self.charset,
        }
    }

    // go: Info.dumpLength L1
    fn dump_length(&self) -> u32 {
        match self.tp {
            // When passing Vector column to the SQL Client, pretend to be a non-binary String.
            // Thus, we use maximum string length here.
            // As a downside, there is no way to get the max dimension of the
            // vector through binary protocol.
            TypeTiDBVectorFloat32 => MaxLongBlobWidth as u32,
            _ => self.column_length,
        }
    }
}

/// DumpFlag dumps flag of a column.
// go: DumpFlag L1
pub fn dump_flag(tp: u8, flag: u16) -> u16 {
    match tp {
        TypeSet => flag | SetFlag as u16,
        TypeEnum => flag | EnumFlag as u16,
        // When passing Vector column to the SQL Client, pretend to be a non-binary String.
        TypeTiDBVectorFloat32 => flag & !(BinaryFlag as u16),
        _ => flag,
    }
}

// go: dumpType L1
fn dump_type(tp: u8) -> u8 {
    match tp {
        TypeSet | TypeEnum => TypeString,
        // When passing Vector column to the SQL Client, pretend to be a non-binary String.
        TypeTiDBVectorFloat32 => TypeLongBlob,
        TypeTinyBlob | TypeMediumBlob | TypeLongBlob => TypeBlob,
        _ => tp,
    }
}

/// DumpTextRow dumps a row to bytes.
// go: DumpTextRow L1
pub fn dump_text_row(
    buffer: &mut Vec<u8>,
    columns: &[Info],
    row: &Row<'_>,
    d: Option<&mut ResultEncoder>,
) -> Result<(), Error> {
    let mut own;
    let d = match d {
        Some(d) => d,
        None => {
            own = ResultEncoder::new(CharsetUTF8MB4);
            &mut own
        }
    };
    let mut tmp: Vec<u8> = Vec::with_capacity(20);
    for (i, col) in columns.iter().enumerate() {
        if row.is_null(i) {
            buffer.push(0xfb);
            continue;
        }
        match col.tp {
            TypeTiny | TypeShort | TypeInt24 | TypeLong => {
                tmp.clear();
                tmp.extend_from_slice(row.get_int64(i).to_string().as_bytes());
                dump::length_encoded_string(buffer, &tmp);
            }
            TypeYear => {
                let year = row.get_int64(i);
                tmp.clear();
                if year == 0 {
                    tmp.extend_from_slice(b"0000");
                } else {
                    tmp.extend_from_slice(year.to_string().as_bytes());
                }
                dump::length_encoded_string(buffer, &tmp);
            }
            TypeLonglong => {
                tmp.clear();
                if has_unsigned_flag(columns[i].flag as usize) {
                    tmp.extend_from_slice(row.get_uint64(i).to_string().as_bytes());
                } else {
                    tmp.extend_from_slice(row.get_int64(i).to_string().as_bytes());
                }
                dump::length_encoded_string(buffer, &tmp);
            }
            TypeFloat => {
                let mut prec = -1;
                if columns[i].decimal > 0
                    && isize::from(col.decimal) != NotFixedDec
                    && col.table.is_empty()
                {
                    prec = isize::from(col.decimal);
                }
                tmp.clear();
                append_format_float(&mut tmp, f64::from(row.get_float32(i)), prec, 32);
                dump::length_encoded_string(buffer, &tmp);
            }
            TypeDouble => {
                let mut prec = UnspecifiedLength;
                if col.decimal > 0
                    && isize::from(col.decimal) != NotFixedDec
                    && col.table.is_empty()
                {
                    prec = isize::from(col.decimal);
                }
                tmp.clear();
                append_format_float(&mut tmp, row.get_float64(i), prec, 64);
                dump::length_encoded_string(buffer, &tmp);
            }
            TypeNewDecimal => {
                dump::length_encoded_string(buffer, row.get_my_decimal(i).string().as_bytes());
            }
            TypeString | TypeVarString | TypeVarchar | TypeBit | TypeTinyBlob | TypeMediumBlob
            | TypeLongBlob | TypeBlob => {
                d.update_data_encoding(col.charset);
                dump::length_encoded_string(buffer, &d.encode_data(row.get_bytes(i)));
            }
            TypeDate | TypeDatetime | TypeTimestamp => {
                dump::length_encoded_string(buffer, &row.get_time(i).string());
            }
            TypeDuration => {
                let dur = row.get_duration(i, isize::from(col.decimal));
                dump::length_encoded_string(buffer, &dur.string());
            }
            TypeEnum => {
                d.update_data_encoding(col.charset);
                dump::length_encoded_string(buffer, &d.encode_data(&row.get_enum(i).string()));
            }
            TypeSet => {
                d.update_data_encoding(col.charset);
                dump::length_encoded_string(buffer, &d.encode_data(&row.get_set(i).string()));
            }
            TypeJSON => {
                // The collation of JSON type is always binary.
                // To compatible with MySQL, here we treat it as utf-8.
                d.update_data_encoding(DefaultCollationID as u16);
                dump::length_encoded_string(buffer, &d.encode_data(&row.get_json(i).string()));
            }
            TypeTiDBVectorFloat32 => {
                d.update_data_encoding(DefaultCollationID as u16);
                dump::length_encoded_string(
                    buffer,
                    &d.encode_data(row.get_vector_float32(i).string().as_bytes()),
                );
            }
            _ => {
                return Err(ErrInvalidType
                    .gen_with_stack("invalid type %v", vec![Arg::from(i64::from(columns[i].tp))]));
            }
        }
    }
    Ok(())
}

/// DumpBinaryRow dumps a row to bytes.
// go: DumpBinaryRow L1
pub fn dump_binary_row(
    buffer: &mut Vec<u8>,
    columns: &[Info],
    row: &Row<'_>,
    d: Option<&mut ResultEncoder>,
) -> Result<(), Error> {
    let mut own;
    let d = match d {
        Some(d) => d,
        None => {
            own = ResultEncoder::new(CharsetUTF8MB4);
            &mut own
        }
    };
    buffer.push(OKHeader);
    let null_bitmap_off = buffer.len();
    let num_bytes4_null = (columns.len() + 7 + 2) / 8;
    buffer.resize(buffer.len() + num_bytes4_null, 0);
    for (i, col) in columns.iter().enumerate() {
        if row.is_null(i) {
            let byte_pos = (i + 2) / 8;
            let bit_pos = ((i + 2) % 8) as u8;
            buffer[null_bitmap_off + byte_pos] |= 1 << bit_pos;
            continue;
        }
        match col.tp {
            TypeTiny => buffer.push(row.get_int64(i) as u8),
            TypeShort | TypeYear => dump::uint16(buffer, row.get_int64(i) as u16),
            TypeInt24 | TypeLong => dump::uint32(buffer, row.get_int64(i) as u32),
            TypeLonglong => dump::uint64(buffer, row.get_uint64(i)),
            TypeFloat => dump::uint32(buffer, row.get_float32(i).to_bits()),
            TypeDouble => dump::uint64(buffer, row.get_float64(i).to_bits()),
            TypeNewDecimal => {
                dump::length_encoded_string(buffer, row.get_my_decimal(i).string().as_bytes());
            }
            TypeString | TypeVarString | TypeVarchar | TypeBit | TypeTinyBlob | TypeMediumBlob
            | TypeLongBlob | TypeBlob => {
                d.update_data_encoding(col.charset);
                dump::length_encoded_string(buffer, &d.encode_data(row.get_bytes(i)));
            }
            TypeDate | TypeDatetime | TypeTimestamp => {
                dump::binary_date_time(buffer, row.get_time(i));
            }
            TypeDuration => {
                buffer.extend_from_slice(&dump::binary_time(row.get_duration(i, 0).duration));
            }
            TypeEnum => {
                d.update_data_encoding(col.charset);
                dump::length_encoded_string(buffer, &d.encode_data(&row.get_enum(i).string()));
            }
            TypeSet => {
                d.update_data_encoding(col.charset);
                dump::length_encoded_string(buffer, &d.encode_data(&row.get_set(i).string()));
            }
            TypeJSON => {
                // The collation of JSON type is always binary.
                // To compatible with MySQL, here we treat it as utf-8.
                d.update_data_encoding(DefaultCollationID as u16);
                dump::length_encoded_string(buffer, &d.encode_data(&row.get_json(i).string()));
            }
            TypeTiDBVectorFloat32 => {
                d.update_data_encoding(DefaultCollationID as u16);
                dump::length_encoded_string(
                    buffer,
                    &d.encode_data(row.get_vector_float32(i).string().as_bytes()),
                );
            }
            _ => {
                return Err(ErrInvalidType
                    .gen_with_stack("invalid type %v", vec![Arg::from(i64::from(col.tp))]));
            }
        }
    }
    Ok(())
}
