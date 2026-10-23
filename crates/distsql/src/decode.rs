//! Decoding of coprocessor responses and of row values read by point gets.

use std::collections::HashMap as StdHashMap;

use protobuf::Message;
use tidb_meta::model::ColumnInfo;
use tidb_meta::model::FieldType;
use tidb_meta::model::TableInfo;
use tidb_meta::model::tp;
use tidb_query_datatype::codec::datum;
use tidb_query_datatype::codec::datum::Datum;
use tidb_query_datatype::codec::row::v2::CODEC_VERSION;
use tidb_query_datatype::codec::row::v2::RowSlice;
use tidb_query_datatype::codec::row::v2::V1CompatibleEncoder;
use tidb_query_datatype::codec::table;
use tidb_query_datatype::expr::EvalContext;
use tipb::SelectResponse;

use crate::Error;
use crate::Result;
use crate::dag::DagContext;
use crate::dag::column_to_pb;

/// Rows of one coprocessor response.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SelectRows {
    pub rows: Vec<Vec<Datum>>,
    /// Warnings TiKV reported (e.g. truncation under `TRUNCATE_AS_WARNING`).
    pub warnings: Vec<String>,
}

fn codec_err(what: &str, e: impl std::fmt::Display) -> Error {
    Error::Codec(format!("{what}: {e}"))
}

/// The tipb column info `unflatten` needs to turn raw datums into typed ones.
fn decode_info(ft: &FieldType) -> tipb::ColumnInfo {
    let mut c = tipb::ColumnInfo::default();
    c.set_tp(i32::from(ft.tp));
    c.set_flag(ft.flag as i32);
    c.set_column_len(ft.flen as i32);
    c.set_decimal(ft.decimal as i32);
    c
}

/// Types whose raw datum is returned as is: `unflatten` does not support them.
fn keep_raw(ft: &FieldType) -> bool {
    matches!(ft.tp, tp::ENUM | tp::SET | tp::BIT)
}

/// Decode a `tipb.SelectResponse` with `EncodeType::TypeDefault` rows of `types`.
/// Date/time and duration values are returned as `Datum::Time` / `Datum::Dur`; enum, set
/// and bit values keep their raw datum.
pub fn decode_select_response(data: &[u8], types: &[FieldType]) -> Result<SelectRows> {
    let mut resp = SelectResponse::default();
    resp.merge_from_bytes(data)
        .map_err(|e| codec_err("decode SelectResponse", e))?;
    if resp.has_error() {
        let e = resp.get_error();
        return Err(Error::Coprocessor {
            code: e.get_code(),
            msg: e.get_msg().to_owned(),
        });
    }
    let infos: Vec<tipb::ColumnInfo> = types.iter().map(decode_info).collect();
    let mut ctx = EvalContext::default();
    let mut rows = Vec::new();
    let mut row = Vec::with_capacity(types.len());
    for chunk in resp.get_chunks() {
        let mut buf = chunk.get_rows_data();
        while !buf.is_empty() {
            let i = row.len();
            let (one, rest) =
                datum::split_datum(buf, false).map_err(|e| codec_err("split datum", e))?;
            buf = rest;
            let mut one = one;
            let d = if keep_raw(&types[i]) {
                datum::decode(&mut one)
                    .map_err(|e| codec_err("decode datum", e))?
                    .pop()
                    .unwrap_or(Datum::Null)
            } else {
                table::decode_col_value(&mut one, &mut ctx, &infos[i])
                    .map_err(|e| codec_err("decode datum", e))?
            };
            row.push(d);
            if row.len() == types.len() {
                rows.push(std::mem::replace(&mut row, Vec::with_capacity(types.len())));
            }
        }
    }
    if !row.is_empty() {
        return Err(Error::Codec(format!(
            "response has a partial row of {} datums, expected {}",
            row.len(),
            types.len()
        )));
    }
    let warnings = resp
        .get_warnings()
        .iter()
        .map(|w| format!("{}: {}", w.get_code(), w.get_msg()))
        .collect();
    Ok(SelectRows { rows, warnings })
}

/// Decode the given columns of the row stored at `t{table.id}_r{handle}` (row format v2 or
/// the old v1). Columns missing from the row (added later) get their origin default.
pub fn decode_row_value(
    table: &TableInfo,
    handle: i64,
    value: &[u8],
    columns: &[&ColumnInfo],
    dag_ctx: &DagContext,
) -> Result<Vec<Datum>> {
    let mut ctx = EvalContext::default();
    let infos: Vec<tipb::ColumnInfo> = columns
        .iter()
        .map(|c| column_to_pb(table, c, dag_ctx))
        .collect::<Result<_>>()?;
    let is_v2 = value.first() == Some(&CODEC_VERSION);
    let v1_values = if is_v2 {
        StdHashMap::new()
    } else {
        let wanted: collections::HashMap<i64, tipb::ColumnInfo> = infos
            .iter()
            .map(|c| (c.get_column_id(), c.clone()))
            .collect();
        let mut data = value;
        table::decode_row(&mut data, &mut ctx, &wanted)
            .map_err(|e| codec_err("decode v1 row", e))?
            .into_iter()
            .collect()
    };
    let row = if is_v2 {
        Some(RowSlice::from_bytes(value).map_err(|e| codec_err("decode v2 row", e))?)
    } else {
        None
    };

    let mut out = Vec::with_capacity(columns.len());
    for (col, info) in columns.iter().zip(&infos) {
        if info.get_pk_handle() {
            out.push(if col.field_type.is_unsigned() {
                Datum::U64(handle as u64)
            } else {
                Datum::I64(handle)
            });
            continue;
        }
        let found = match &row {
            Some(row) => match row.get(col.id).map_err(|e| codec_err("row v2", e))? {
                Some(bytes) => {
                    let mut buf = Vec::with_capacity(bytes.len() + 1);
                    buf.write_v2_as_datum(bytes, info)
                        .map_err(|e| codec_err("row v2 column", e))?;
                    let mut slice = buf.as_slice();
                    Some(
                        table::decode_col_value(&mut slice, &mut ctx, info)
                            .map_err(|e| codec_err("row v2 column", e))?,
                    )
                }
                None if row.search_in_null_ids(col.id) => Some(Datum::Null),
                None => None,
            },
            None => v1_values.get(&col.id).cloned(),
        };
        let d = match found {
            Some(d) => d,
            None if info.has_default_val() => {
                let mut slice = info.get_default_val();
                table::decode_col_value(&mut slice, &mut ctx, info)
                    .map_err(|e| codec_err("origin default", e))?
            }
            None => Datum::Null,
        };
        out.push(d);
    }
    Ok(out)
}
