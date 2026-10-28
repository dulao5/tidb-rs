//! Coprocessor reads of integer-handle tables.
//!
//! - [`ranges`]: record key ranges `t{table_id}_r{handle}` from handle ranges.
//! - [`dag`]: tipb `DAGRequest` (table scan, partial aggregation, limit) as Go TiDB builds it.
//! - [`decode`]: `SelectResponse` rows and point-get row values into datums.
//! - [`select`] / [`point_get`]: run them at a [`tidb_kv::KvSnapshot`].

#![forbid(unsafe_code)]

pub mod dag;
pub mod decode;
pub mod ranges;

use tidb_kv::CopRequest;
use tidb_kv::KvSnapshot;
use tidb_kv::REQ_TYPE_DAG;
use tidb_meta::model::ColumnInfo;
use tidb_meta::model::TableInfo;
pub use tidb_query_datatype::codec::datum::Datum;

pub use crate::dag::AggFunc;
pub use crate::dag::DagContext;
pub use crate::dag::DagPlan;
pub use crate::dag::DagSpec;
pub use crate::dag::build_dag;
pub use crate::decode::SelectRows;
pub use crate::decode::decode_row_value;
pub use crate::decode::decode_select_response;
pub use crate::ranges::HandleRange;
pub use crate::ranges::record_range;
pub use crate::ranges::record_ranges;
pub use crate::ranges::row_key;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Kv(#[from] tidb_kv::Error),
    /// The table, column or plan shape is not supported by this node.
    #[error("unsupported: {0}")]
    Unsupported(String),
    /// TiKV's coprocessor returned an error in the response.
    #[error("coprocessor error {code}: {msg}")]
    Coprocessor { code: i32, msg: String },
    #[error("codec error: {0}")]
    Codec(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Options of a coprocessor read.
#[derive(Clone, Copy, Debug, Default)]
pub struct SelectOptions {
    /// Return rows in handle order (descending if the plan scans `desc`).
    pub keep_order: bool,
    /// Region tasks in flight; `None` for the client default (15, TiDB's
    /// `tidb_distsql_scan_concurrency`).
    pub concurrency: Option<usize>,
    /// TiKV paging `(min_rows, max_rows)`; TiDB uses (128, 50000) (`tidb_min_paging_size`,
    /// `tidb_max_paging_size`) for paging-enabled readers.
    pub paging: Option<(u64, u64)>,
    /// Schema version the plan was built against (`coprocessor.Request.schema_ver`).
    pub schema_version: i64,
}

/// Run `plan` over `ranges` at the snapshot and decode all rows. With partial aggregation
/// there is one partial result per region (page), to be merged by the caller.
pub async fn select(
    snapshot: &mut KvSnapshot,
    plan: &DagPlan,
    ranges: Vec<tidb_kv::KeyRange>,
    options: SelectOptions,
) -> Result<SelectRows> {
    let desc = plan
        .request
        .get_executors()
        .first()
        .is_some_and(|e| e.get_tbl_scan().get_desc());
    let mut req = CopRequest::new(
        REQ_TYPE_DAG,
        plan.encode()?,
        ranges.into_iter().map(Into::into).collect(),
    )
    .keep_order(options.keep_order)
    .desc(desc)
    .schema_ver(options.schema_version);
    if let Some(c) = options.concurrency {
        req = req.concurrency(c);
    }
    if let Some((min, max)) = options.paging {
        req = req.paging(min, max);
    }
    let paging = options.paging.is_some();
    let start = std::time::Instant::now();
    let responses = snapshot.coprocessor(req).await?.collect_all().await?;
    // Go `selectResult.fetchResp`: the time spent waiting for coprocessor responses, recorded
    // once the last one arrived.
    tidb_metrics::distsql_query(start.elapsed(), paging);
    let mut out = SelectRows::default();
    for resp in responses {
        let part = decode_select_response(&resp.data, &plan.output_types)?;
        out.rows.extend(part.rows);
        out.warnings.extend(part.warnings);
    }
    Ok(out)
}

/// Read the row with integer handle `handle` (a KV point get, as TiDB's PointGet does) and
/// decode `columns`. `None` if the row does not exist.
pub async fn point_get(
    snapshot: &mut KvSnapshot,
    table: &TableInfo,
    handle: i64,
    columns: &[&ColumnInfo],
    ctx: &DagContext,
) -> Result<Option<Vec<Datum>>> {
    table
        .check_int_handle_readable()
        .map_err(|e| Error::Unsupported(e.to_string()))?;
    let Some(value) = snapshot.get(row_key(table.id, handle)).await? else {
        return Ok(None);
    };
    decode_row_value(table, handle, &value, columns, ctx).map(Some)
}

/// Read several rows by handle with one batch get; result in the order of `handles`,
/// skipping missing rows.
pub async fn batch_point_get(
    snapshot: &mut KvSnapshot,
    table: &TableInfo,
    handles: &[i64],
    columns: &[&ColumnInfo],
    ctx: &DagContext,
) -> Result<Vec<Vec<Datum>>> {
    table
        .check_int_handle_readable()
        .map_err(|e| Error::Unsupported(e.to_string()))?;
    let keys: Vec<Vec<u8>> = handles.iter().map(|&h| row_key(table.id, h)).collect();
    let values: std::collections::HashMap<Vec<u8>, Vec<u8>> = snapshot
        .batch_get(keys.clone())
        .await?
        .into_iter()
        .collect();
    let mut out = Vec::new();
    for (key, &h) in keys.iter().zip(handles) {
        if let Some(v) = values.get(key) {
            out.push(decode_row_value(table, h, v, columns, ctx)?);
        }
    }
    Ok(out)
}
