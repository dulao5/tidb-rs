//! go-file: pkg/sessionctx/variable/session.go
//!
//! The methods of `variable.Concurrency` (block D05.2). Go embeds `Concurrency` in
//! `SessionVars`; its fields are flattened into [`SessionVarsFields`] (rsgen), so the methods
//! are on that struct and reached through `SessionVars`'s `Deref`. Go `int` is `i64`.

use super::session_fields::SessionVarsFields;
use super::tidb_vars::ConcurrencyUnset;

impl SessionVarsFields {
    /// SetIndexLookupConcurrency set the number of concurrent index lookup worker.
    // go: Concurrency.SetIndexLookupConcurrency L1
    pub fn set_index_lookup_concurrency(&mut self, n: i64) {
        self.index_lookup_concurrency = n;
    }

    /// SetIndexLookupJoinConcurrency set the number of concurrent index lookup join inner worker.
    // go: Concurrency.SetIndexLookupJoinConcurrency L1
    pub fn set_index_lookup_join_concurrency(&mut self, n: i64) {
        self.index_lookup_join_concurrency = n;
    }

    /// SetDistSQLScanConcurrency set the number of concurrent dist SQL scan worker.
    // go: Concurrency.SetDistSQLScanConcurrency L1
    pub fn set_dist_sql_scan_concurrency(&mut self, n: i64) {
        self.dist_sql_scan_concurrency = n;
    }

    /// SetAnalyzeDistSQLScanConcurrency set the number of concurrent dist SQL scan worker when to analyze.
    // go: Concurrency.SetAnalyzeDistSQLScanConcurrency L1
    pub fn set_analyze_dist_sql_scan_concurrency(&mut self, n: i64) {
        self.analyze_dist_sql_scan_concurrency = n;
    }

    /// SetHashJoinConcurrency set the number of concurrent hash join outer worker.
    // go: Concurrency.SetHashJoinConcurrency L1
    pub fn set_hash_join_concurrency(&mut self, n: i64) {
        self.hash_join_concurrency = n;
    }

    /// SetProjectionConcurrency set the number of concurrent projection worker.
    // go: Concurrency.SetProjectionConcurrency L1
    pub fn set_projection_concurrency(&mut self, n: i64) {
        self.projection_concurrency = n;
    }

    /// SetHashAggPartialConcurrency set the number of concurrent hash aggregation partial worker.
    // go: Concurrency.SetHashAggPartialConcurrency L1
    pub fn set_hash_agg_partial_concurrency(&mut self, n: i64) {
        self.hash_agg_partial_concurrency = n;
    }

    /// SetHashAggFinalConcurrency set the number of concurrent hash aggregation final worker.
    // go: Concurrency.SetHashAggFinalConcurrency L1
    pub fn set_hash_agg_final_concurrency(&mut self, n: i64) {
        self.hash_agg_final_concurrency = n;
    }

    /// SetWindowConcurrency set the number of concurrent window worker.
    // go: Concurrency.SetWindowConcurrency L1
    pub fn set_window_concurrency(&mut self, n: i64) {
        self.window_concurrency = n;
    }

    /// SetMergeJoinConcurrency set the number of concurrent merge join worker.
    // go: Concurrency.SetMergeJoinConcurrency L1
    pub fn set_merge_join_concurrency(&mut self, n: i64) {
        self.merge_join_concurrency = n;
    }

    /// SetStreamAggConcurrency set the number of concurrent stream aggregation worker.
    // go: Concurrency.SetStreamAggConcurrency L1
    pub fn set_stream_agg_concurrency(&mut self, n: i64) {
        self.stream_agg_concurrency = n;
    }

    /// SetIndexMergeIntersectionConcurrency set the number of concurrent intersection process worker.
    // go: Concurrency.SetIndexMergeIntersectionConcurrency L1
    pub fn set_index_merge_intersection_concurrency(&mut self, n: i64) {
        self.index_merge_intersection_concurrency = n;
    }

    /// SetIndexSerialScanConcurrency set the number of concurrent index serial scan worker.
    // go: Concurrency.SetIndexSerialScanConcurrency L1
    pub fn set_index_serial_scan_concurrency(&mut self, n: i64) {
        self.index_serial_scan_concurrency = n;
    }

    /// IndexLookupConcurrency return the number of concurrent index lookup worker.
    // go: Concurrency.IndexLookupConcurrency L1
    pub fn index_lookup_concurrency(&self) -> i64 {
        if self.index_lookup_concurrency != ConcurrencyUnset {
            return self.index_lookup_concurrency;
        }
        self.executor_concurrency
    }

    /// IndexLookupJoinConcurrency return the number of concurrent index lookup join inner worker.
    // go: Concurrency.IndexLookupJoinConcurrency L1
    pub fn index_lookup_join_concurrency(&self) -> i64 {
        if self.index_lookup_join_concurrency != ConcurrencyUnset {
            return self.index_lookup_join_concurrency;
        }
        self.executor_concurrency
    }

    /// DistSQLScanConcurrency return the number of concurrent dist SQL scan worker.
    // go: Concurrency.DistSQLScanConcurrency L1
    pub fn dist_sql_scan_concurrency(&self) -> i64 {
        self.dist_sql_scan_concurrency
    }

    /// AnalyzeDistSQLScanConcurrency return the number of concurrent dist SQL scan worker when to analyze.
    // go: Concurrency.AnalyzeDistSQLScanConcurrency L1
    pub fn analyze_dist_sql_scan_concurrency(&self) -> i64 {
        self.analyze_dist_sql_scan_concurrency
    }

    /// HashJoinConcurrency return the number of concurrent hash join outer worker.
    // go: Concurrency.HashJoinConcurrency L1
    pub fn hash_join_concurrency(&self) -> i64 {
        if self.hash_join_concurrency != ConcurrencyUnset {
            return self.hash_join_concurrency;
        }
        self.executor_concurrency
    }

    /// ProjectionConcurrency return the number of concurrent projection worker.
    // go: Concurrency.ProjectionConcurrency L1
    pub fn projection_concurrency(&self) -> i64 {
        if self.projection_concurrency != ConcurrencyUnset {
            return self.projection_concurrency;
        }
        self.executor_concurrency
    }

    /// HashAggPartialConcurrency return the number of concurrent hash aggregation partial worker.
    // go: Concurrency.HashAggPartialConcurrency L1
    pub fn hash_agg_partial_concurrency(&self) -> i64 {
        if self.hash_agg_partial_concurrency != ConcurrencyUnset {
            return self.hash_agg_partial_concurrency;
        }
        self.executor_concurrency
    }

    /// HashAggFinalConcurrency return the number of concurrent hash aggregation final worker.
    // go: Concurrency.HashAggFinalConcurrency L1
    pub fn hash_agg_final_concurrency(&self) -> i64 {
        if self.hash_agg_final_concurrency != ConcurrencyUnset {
            return self.hash_agg_final_concurrency;
        }
        self.executor_concurrency
    }

    /// WindowConcurrency return the number of concurrent window worker.
    // go: Concurrency.WindowConcurrency L1
    pub fn window_concurrency(&self) -> i64 {
        if self.window_concurrency != ConcurrencyUnset {
            return self.window_concurrency;
        }
        self.executor_concurrency
    }

    /// MergeJoinConcurrency return the number of concurrent merge join worker.
    // go: Concurrency.MergeJoinConcurrency L1
    pub fn merge_join_concurrency(&self) -> i64 {
        if self.merge_join_concurrency != ConcurrencyUnset {
            return self.merge_join_concurrency;
        }
        self.executor_concurrency
    }

    /// StreamAggConcurrency return the number of concurrent stream aggregation worker.
    // go: Concurrency.StreamAggConcurrency L1
    pub fn stream_agg_concurrency(&self) -> i64 {
        if self.stream_agg_concurrency != ConcurrencyUnset {
            return self.stream_agg_concurrency;
        }
        self.executor_concurrency
    }

    /// IndexMergeIntersectionConcurrency return the number of concurrent process worker.
    // go: Concurrency.IndexMergeIntersectionConcurrency L1
    pub fn index_merge_intersection_concurrency(&self) -> i64 {
        if self.index_merge_intersection_concurrency != ConcurrencyUnset {
            return self.index_merge_intersection_concurrency;
        }
        self.executor_concurrency
    }

    /// IndexSerialScanConcurrency return the number of concurrent index serial scan worker.
    /// This option is not sync with ExecutorConcurrency since it's used by Analyze table.
    // go: Concurrency.IndexSerialScanConcurrency L1
    pub fn index_serial_scan_concurrency(&self) -> i64 {
        self.index_serial_scan_concurrency
    }

    /// UnionConcurrency return the num of concurrent union worker.
    // go: Concurrency.UnionConcurrency L1
    pub fn union_concurrency(&self) -> i64 {
        self.executor_concurrency
    }
}
