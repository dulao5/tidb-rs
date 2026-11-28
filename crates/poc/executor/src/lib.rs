//! Root executors for [`tidb_sql::Plan`]s: read the source (KV point get or coprocessor table
//! reader), then run the root operators over materialised rows: hash aggregation, sort, limit
//! and projection to protocol values.

#![forbid(unsafe_code)]

pub mod agg;
pub mod compare;
pub mod convert;

use std::cmp::Ordering;
use std::future::Future;
use std::sync::Arc;

use poc_kv::KvSnapshot;
use tidb_distsql::DagContext;
use tidb_distsql::DagSpec;
use tidb_distsql::Datum;
use tidb_distsql::SelectOptions;
use tidb_distsql::split_ranges_across_int64_boundary;
use tidb_memory::sqlkiller::SqlKiller;
use tidb_mysql_protocol::SqlError;
use tidb_mysql_protocol::Value;
use tidb_runtime::CheckpointHook;
use tidb_runtime::cancel::KillCancel;
use tidb_sql::Plan;
use tidb_sql::plan::Source;

use crate::compare::StrCmp;

/// Statement-level settings of an execution.
#[derive(Clone, Debug)]
pub struct ExecContext {
    pub dag: DagContext,
    /// Schema version the plan was built against (sent with coprocessor requests).
    pub schema_version: i64,
    /// Optional kill signal checked across `poc-executor` checkpoints and KV reads.
    pub killer: Option<Arc<SqlKiller>>,
    /// Resettable session cancellation notifier paired with [`Self::killer`].
    pub kill_cancel: Option<KillCancel>,
}

struct PocKillHook<'a>(&'a SqlKiller);

impl CheckpointHook for PocKillHook<'_> {
    type Error = tidb_base::Error;

    fn check(&self) -> Result<(), Self::Error> {
        self.0.handle_signal()
    }
}

async fn check_killed(cx: &ExecContext) -> Result<(), SqlError> {
    if let Some(killer) = &cx.killer {
        if let Err(err) = tidb_runtime::checkpoint(&PocKillHook(killer.as_ref())).await {
            return Err(SqlError::from_tidb_error(&err));
        }
    } else {
        let Ok(()) = tidb_runtime::checkpoint(&()).await;
    }
    Ok(())
}

async fn cancel_select<T>(
    cx: &ExecContext,
    fut: impl Future<Output = Result<T, SqlError>>,
) -> Result<T, SqlError> {
    check_killed(cx).await?;
    let Some(cancel) = &cx.kill_cancel else {
        return fut.await;
    };
    tokio::select! {
        biased;
        () = cancel.cancelled() => {
            if let Some(killer) = &cx.killer
                && let Err(err) = killer.handle_signal()
            {
                return Err(SqlError::from_tidb_error(&err));
            }
            Err(SqlError::new(
                tidb_base::errno::ErrQueryInterrupted,
                "Query execution was interrupted",
            ))
        }
        res = fut => res,
    }
}

/// Maps a read-path error to a client error.
pub fn distsql_error(e: tidb_distsql::Error) -> SqlError {
    match e {
        tidb_distsql::Error::Unsupported(s) => SqlError::not_supported(s),
        tidb_distsql::Error::Killed(err) => SqlError::from_tidb_error(&err),
        tidb_distsql::Error::Kv(poc_kv::Error::Copr(err)) => SqlError::from_tidb_error(&err),
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
            cancel_select(cx, async {
                let row =
                    tidb_distsql::point_get(snapshot, &plan.table, *handle, &columns, &cx.dag)
                        .await
                        .map_err(distsql_error)?;
                Ok(row.into_iter().collect())
            })
            .await
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
                check_killed(cx).await?;
                // go: distsql.tableRangesToKVRangesWithoutSplit: one key range per handle
                // range, in the given order.
                let key_ranges: Vec<_> = part
                    .iter()
                    .filter_map(|r| tidb_distsql::record_range(plan.table.id, r.to_handle_range()))
                    .collect();
                if key_ranges.is_empty() {
                    continue;
                }
                let part_rows = cancel_select(cx, async {
                    tidb_distsql::select_with_killer(
                        snapshot,
                        &dag,
                        key_ranges,
                        options,
                        cx.killer.clone(),
                        cx.kill_cancel.clone(),
                    )
                    .await
                    .map_err(distsql_error)
                })
                .await?;
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
    check_killed(cx).await?;
    finish(plan, rows, cx.dag.new_collation_enabled)
}
