//! go-file: pkg/executor/internal/exec/indexusage.go
//!
//! IndexUsageReporter: the index usage samples of reader executors (`SHOW INDEX USAGE`,
//! `sys.schema_unused_indexes`). The collector (`indexusage.StmtIndexUsageCollector`) and the
//! statement's used statistics (`stmtctx.UsedStatsInfo`) are statistics (D11.2): until then
//! no table has a row count, so every report is skipped, as Go skips a table without valid
//! statistics.

use std::sync::Arc;

use tidb_execdetails::execdetails::RuntimeStatsColl;
use tidb_model::TableInfo;

/// IndexUsageReporter is a toolkit to report index usage
///
/// `reporter` (`*indexusage.StmtIndexUsageCollector`) and `statsMap`
/// (`*stmtctx.UsedStatsInfo`) are D11.2's.
pub struct IndexUsageReporter {
    runtime_stats_coll: Option<Arc<RuntimeStatsColl>>,
}

/// NewIndexUsageReporter creates an index usage reporter util
// go: NewIndexUsageReporter L2 (the collector and the used statistics are D11.2)
pub fn new_index_usage_reporter(
    runtime_stats_coll: Option<Arc<RuntimeStatsColl>>,
) -> IndexUsageReporter {
    IndexUsageReporter { runtime_stats_coll }
}

impl IndexUsageReporter {
    /// ReportCopIndexUsageForHandle wraps around `ReportCopIndexUsageForTable` to get the
    /// `indexID` automatically from the `table.Table` if the table has a clustered index or
    /// integer primary key.
    ///
    /// `table.Table` (D06.4) is its `Meta()` and, for a partition, its physical ID.
    // go: IndexUsageReporter.ReportCopIndexUsageForHandle L2 (table.Table as its meta and physical ID)
    pub fn report_cop_index_usage_for_handle(
        &self,
        tbl: &TableInfo,
        physical_id: Option<i64>,
        plan_id: isize,
    ) {
        let Some(idx_id) = get_cluster_index_id(tbl) else {
            return;
        };

        self.report_cop_index_usage_for_table(tbl, physical_id, idx_id, plan_id);
    }

    /// ReportCopIndexUsageForTable wraps around `ReportCopIndexUsage` to get `tableID` and
    /// `physicalTableID` from the `table.Table`. If it's expected to calculate the percentage
    /// according to the size of partition, the `tbl` argument should be a
    /// `table.PhysicalTable`, or the percentage will be calculated using the size of whole
    /// table.
    // go: IndexUsageReporter.ReportCopIndexUsageForTable L2 (table.Table as its meta and physical ID)
    pub fn report_cop_index_usage_for_table(
        &self,
        tbl: &TableInfo,
        physical_id: Option<i64>,
        index_id: i64,
        plan_id: isize,
    ) {
        let table_id = tbl.id;
        let mut physical_table_id = table_id;
        if let Some(id) = physical_id {
            physical_table_id = id;
        }

        self.report_cop_index_usage(table_id, physical_table_id, index_id, plan_id);
    }

    /// ReportCopIndexUsage reports the index usage to the inside collector. The index usage
    /// will be recorded in the `tableID+indexID`, but the percentage is calculated using the
    /// size of the table specified by `physicalTableID`.
    // go: IndexUsageReporter.ReportCopIndexUsage L1
    pub fn report_cop_index_usage(
        &self,
        table_id: i64,
        physical_table_id: i64,
        index_id: i64,
        plan_id: isize,
    ) {
        let Some(table_row_count) = self.get_table_row_count(physical_table_id) else {
            // skip if the table is empty or the stats is not valid
            return;
        };

        let (kv_req, access_rows) = match &self.runtime_stats_coll {
            Some(c) => c.get_cop_count_and_rows(plan_id as i64),
            None => (0, 0),
        };
        if kv_req == 0 && access_rows == 0 {
            return;
        }

        // indexusage.NewSample(0, kvReq, accessRows, tableRowCount) and e.reporter.Update:
        // D11.2 (unreachable until it provides the row count).
        let _ = (table_id, index_id, table_row_count);
    }

    /// ReportPointGetIndexUsageForHandle wraps around `ReportPointGetIndexUsage` to get the
    /// `indexID` automatically from the `table.Table` if the table has a clustered index or
    /// integer primary key.
    // go: IndexUsageReporter.ReportPointGetIndexUsageForHandle L1
    pub fn report_point_get_index_usage_for_handle(
        &self,
        tbl_info: &TableInfo,
        physical_table_id: i64,
        kv_request_total: i64,
        rows: i64,
    ) {
        let Some(idx_id) = get_cluster_index_id(tbl_info) else {
            return;
        };

        self.report_point_get_index_usage(
            tbl_info.id,
            physical_table_id,
            idx_id,
            kv_request_total,
            rows,
        );
    }

    /// ReportPointGetIndexUsage reports the index usage of a point get or batch point get
    // go: IndexUsageReporter.ReportPointGetIndexUsage L1
    pub fn report_point_get_index_usage(
        &self,
        table_id: i64,
        physical_table_id: i64,
        index_id: i64,
        kv_request_total: i64,
        rows: i64,
    ) {
        let Some(table_row_count) = self.get_table_row_count(physical_table_id) else {
            // skip if the table is empty or the stats is not valid
            return;
        };

        // indexusage.NewSample / e.reporter.Update: D11.2.
        let _ = (table_id, index_id, kv_request_total, rows, table_row_count);
    }

    /// getTableRowCount returns the `RealtimeCount` of a table
    // go: IndexUsageReporter.getTableRowCount L2 (boundary stub, blocked-by:D11.2)
    fn get_table_row_count(&self, table_id: i64) -> Option<i64> {
        let _ = table_id;
        // e.statsMap.GetUsedInfo(tableID): no statement records used statistics before D11.2
        // (Go: `stats == nil` → `(0, false)`).
        // glue: stub, blocked-by:D11.2 (stmtctx.UsedStatsInfo)
        None
    }
}

/// getClusterIndexID returns the indexID of the clustered index. If the table doesn't have a
/// clustered index, it returns (0, false).
// go: getClusterIndexID L1
fn get_cluster_index_id(tbl_info: &TableInfo) -> Option<i64> {
    let mut idx_id = 0;
    if tbl_info.pk_is_handle {
        idx_id = 0;
    } else if tbl_info.is_common_handle {
        for idx in tbl_info.indices.iter().flatten() {
            if idx.primary {
                idx_id = idx.id;
            }
        }
    } else {
        // just ignore, this table is read through rowid.
        return None;
    }

    Some(idx_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// After Go indexusage_test.go: the clustered index ID of int-handle, common-handle and
    /// row-ID tables.
    #[test]
    fn cluster_index_id() {
        let mut t = TableInfo {
            pk_is_handle: true,
            ..Default::default()
        };
        assert_eq!(get_cluster_index_id(&t), Some(0));
        t.pk_is_handle = false;
        assert_eq!(get_cluster_index_id(&t), None);
        t.is_common_handle = true;
        t.indices = Some(vec![
            tidb_model::IndexInfo {
                id: 1,
                ..Default::default()
            },
            tidb_model::IndexInfo {
                id: 2,
                primary: true,
                ..Default::default()
            },
        ]);
        assert_eq!(get_cluster_index_id(&t), Some(2));
        // Without statistics (D11.2) every report is skipped.
        let r = new_index_usage_reporter(None);
        r.report_point_get_index_usage_for_handle(&t, 1, 1, 1);
        r.report_cop_index_usage_for_handle(&t, None, 1);
    }
}
