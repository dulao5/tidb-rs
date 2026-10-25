//! Root executors for [`tidb_sql::Plan`]s: read the source (KV point get or coprocessor table
//! reader), then run the root operators over materialised rows: hash aggregation, sort, limit
//! and projection to protocol values.

#![forbid(unsafe_code)]

pub mod agg;
pub mod compare;
pub mod convert;

use std::cmp::Ordering;

use tidb_distsql::DagContext;
use tidb_distsql::DagSpec;
use tidb_distsql::Datum;
use tidb_distsql::HandleRange;
use tidb_distsql::SelectOptions;
use tidb_kv::KvSnapshot;
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

/// Maps a read-path error to a client error.
pub fn distsql_error(e: tidb_distsql::Error) -> SqlError {
    match e {
        tidb_distsql::Error::Unsupported(s) => SqlError::not_supported(s),
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
            lo,
            hi,
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
            let Some(range) =
                tidb_distsql::record_range(plan.table.id, HandleRange::inclusive(*lo, *hi))
            else {
                return Ok(Vec::new());
            };
            let options = SelectOptions {
                keep_order: *keep_order,
                schema_version: cx.schema_version,
                ..SelectOptions::default()
            };
            let rows = tidb_distsql::select(snapshot, &dag, vec![range], options)
                .await
                .map_err(distsql_error)?;
            Ok(rows.rows)
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
    finish(plan, rows, cx.dag.new_collation_enabled)
}
