//! Root executors for [`tidb_sql::Plan`]s: read the source (KV point get or coprocessor table
//! reader), then run the root operators over materialised rows: hash aggregation, sort, limit
//! and projection to protocol values.

#![forbid(unsafe_code)]

pub mod agg;
pub mod compare;
pub mod convert;

use std::cmp::Ordering;

use poc_kv::KvSnapshot;
use tidb_distsql::DagContext;
use tidb_distsql::DagSpec;
use tidb_distsql::Datum;
use tidb_distsql::SelectOptions;
use tidb_distsql::split_ranges_across_int64_boundary;
use tidb_mysql_protocol::SqlError;
use tidb_mysql_protocol::Value;
use tidb_sql::Plan;
use tidb_sql::plan::Source;

use crate::compare::StrCmp;

/// Statement-level settings of an execution.
#[derive(Clone, Debug)]
pub struct ExecContext {
    pub dag: DagContext,
    /// Schema version the plan was built against (sent with coprocessor requests).
    pub schema_version: i64,
}

/// Maps a read-path error to a client error, preserving normalized TiDB error codes
/// (such as `9006` `ErrGCTooEarly` and `9001` `ErrPDServerTimeout`).
pub fn distsql_error(e: tidb_distsql::Error) -> SqlError {
    match e {
        tidb_distsql::Error::Unsupported(s) => SqlError::not_supported(s),
        tidb_distsql::Error::Kv(ref kv) => match kv.as_tidb_error() {
            Some(err) => SqlError::from_tidb_error(err),
            None => SqlError::unknown(kv.to_string()),
        },
        other => SqlError::unknown(other.to_string()),
    }
}

/// Reads the plan's source rows (the scanned columns, or TiKV partial aggregation rows).
pub async fn read_source(
    plan: &Plan,
    snapshot: &mut KvSnapshot,
    cx: &ExecContext,
) -> Result<Vec<Vec<Datum>>, SqlError> {
    let columns = plan.scan_column_infos();
    match &plan.source {
        Source::Dual => Ok(Vec::new()),
        Source::PointGet { handle } => {
            let row = tidb_distsql::point_get(snapshot, &plan.table, *handle, &columns, &cx.dag)
                .await
                .map_err(distsql_error)?;
            Ok(row.into_iter().collect())
        }
        Source::TableReader {
            range,
            desc,
            keep_order,
            limit,
            agg,
        } => {
            let spec = DagSpec {
                table: &plan.table,
                columns,
                desc: *desc,
                aggregation: agg.as_ref().map(|a| (a.funcs.clone(), a.group_by.clone())),
                limit: *limit,
            };
            let dag = tidb_distsql::build_dag(&spec, &cx.dag).map_err(distsql_error)?;
            let options = SelectOptions {
                // PoC: a read without aggregation collects its responses in key order even when
                // the plan does not need handle order, so a LIMIT without ORDER BY returns the
                // rows Go returns in practice (the first regions' rows). The range split below
                // follows the plan's `keep_order`, as Go's.
                keep_order: *keep_order || agg.is_none(),
                schema_version: cx.schema_version,
                ..SelectOptions::default()
            };
            // go: TableReaderExecutor.Open: an UNSIGNED handle range is split at the int64
            // boundary; with keep-order the two parts are separate requests, read one after
            // the other (`tableResultHandler`).
            let (first, second) = split_ranges_across_int64_boundary(&[*range], *keep_order, *desc);
            let mut rows = Vec::new();
            for part in [first, second] {
                // go: distsql.tableRangesToKVRangesWithoutSplit: one key range per handle
                // range, in the given order.
                let key_ranges: Vec<_> = part
                    .iter()
                    .filter_map(|r| tidb_distsql::record_range(plan.table.id, r.to_handle_range()))
                    .collect();
                if key_ranges.is_empty() {
                    continue;
                }
                let part_rows = tidb_distsql::select(snapshot, &dag, key_ranges, options)
                    .await
                    .map_err(distsql_error)?;
                rows.extend(part_rows.rows);
            }
            Ok(rows)
        }
    }
}

/// Runs the root operators over the source rows and converts the output to protocol values.
pub fn finish(
    plan: &Plan,
    mut rows: Vec<Vec<Datum>>,
    new_collation: bool,
) -> Result<Vec<Vec<Value>>, SqlError> {
    if let Some(agg) = &plan.agg {
        rows = agg::aggregate(agg, rows, new_collation)?;
    }
    if !plan.sort.is_empty() {
        let keys: Vec<(usize, bool, StrCmp)> = plan
            .sort
            .iter()
            .map(|k| Ok((k.col, k.desc, StrCmp::of(&k.ft, new_collation)?)))
            .collect::<Result<_, SqlError>>()?;
        let mut err = None;
        rows.sort_by(|a, b| {
            for &(col, desc, cmp) in &keys {
                match compare::compare(&a[col], &b[col], cmp) {
                    Ok(Ordering::Equal) => continue,
                    Ok(o) => return if desc { o.reverse() } else { o },
                    Err(e) => {
                        err.get_or_insert(e);
                        return Ordering::Equal;
                    }
                }
            }
            Ordering::Equal
        });
        if let Some(e) = err {
            return Err(e);
        }
    }
    let rows: Box<dyn Iterator<Item = Vec<Datum>>> = match plan.limit {
        Some(l) => Box::new(
            rows.into_iter()
                .skip(usize::try_from(l.offset).unwrap_or(usize::MAX))
                .take(usize::try_from(l.count).unwrap_or(usize::MAX)),
        ),
        None => Box::new(rows.into_iter()),
    };
    rows.map(|row| {
        plan.output
            .iter()
            .zip(&plan.output_types)
            .map(|(&i, ft)| convert::to_value(row[i].clone(), ft))
            .collect()
    })
    .collect()
}

/// Executes a plan at the snapshot.
pub async fn execute(
    plan: &Plan,
    snapshot: &mut KvSnapshot,
    cx: &ExecContext,
) -> Result<Vec<Vec<Value>>, SqlError> {
    let rows = read_source(plan, snapshot, cx).await?;
    // N3 checkpoint between reading and the root operators (aggregation, sort), which run
    // without yielding in the PoC.
    let Ok(()) = tidb_runtime::checkpoint(&()).await;
    finish(plan, rows, cx.dag.new_collation_enabled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn distsql_error_preserves_safepoint_9006_and_9001() {
        let cache = poc_kv::SafePointCache::new();
        let sp = (1_700_000_060_000_u64 << 18) + 1;
        cache.update(sp, Instant::now());
        let e_9006 = cache.check_visibility(sp - 10).unwrap_err();
        let sql_9006 = distsql_error(tidb_distsql::Error::Kv(poc_kv::Error::SafePoint(Box::new(
            e_9006.clone(),
        ))));
        assert_eq!(sql_9006.code, 9006);
        assert_eq!(sql_9006.state, "HY000");
        assert_eq!(sql_9006.message, String::from_utf8_lossy(&e_9006.get_msg()));
        assert!(sql_9006.source_err.is_some());

        let stale = poc_kv::SafePointCache::new();
        stale.update(10, Instant::now() - Duration::from_secs(100));
        let e_9001 = stale.check_visibility(20).unwrap_err();
        let sql_9001 = distsql_error(tidb_distsql::Error::Kv(poc_kv::Error::Copr(e_9001)));
        assert_eq!(sql_9001.code, 9001);
        assert_eq!(sql_9001.state, "HY000");
        assert_eq!(
            sql_9001.message,
            "PD server timeout: start timestamp may fall behind safe point"
        );
    }
}
