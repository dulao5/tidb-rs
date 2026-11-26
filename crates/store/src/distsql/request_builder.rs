//! go-file: pkg/distsql/request_builder.go
//!
//! Port of Go TiDB v8.5.8 `pkg/distsql/request_builder.go` (block D07.3, Part 1: 40 functions):
//! `RequestBuilder` (TableReader read-path setters, `Build`, `verifyTxnScope`), handle /
//! common-handle range encoding, `SplitRangesAcrossInt64Boundary`, `VerifyTxnScope`, and
//! `EncodeIndexKey`.

// Package-level consts keep their Go names (mapping rule M-GONAME).
#![allow(non_upper_case_globals)]

use std::collections::BTreeSet;
use std::sync::Arc;

use protobuf::Message;
use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_base::gostd::sort;
use tidb_base::gostd::time::utc;
use tidb_base::kv::StoreType;
use tidb_base::mysql::{DelayedPriority, HighPriority, LowPriority, NoPriority};
use tidb_codec::codec::encode_key;
use tidb_codec::number::encode_int;
use tidb_infoschema::context::MetaOnlyInfoSchema;
use tidb_kv::copr::{RateLimit, ResourceGroupTagBuilder, StoreLabel};
use tidb_kv::key::{Key, KeyRange};
use tidb_kv::kv::{
    CoprRequestAdjuster, GlobalReplicaScope, IsoLevel, KeyRanges, PriorityHigh, PriorityLow,
    PriorityNormal, RC, RCCheckTS, ReqTypeAnalyze, ReqTypeDAG, Request, SI,
    new_non_paritioned_key_ranges_with_hint, new_non_partitioned_key_ranges,
    new_partitioned_key_ranges,
};
use tidb_kv::option::ReplicaReadLeader;
use tidb_kv::txn_scope_var::GlobalTxnScope;
use tidb_memory::tracker::Tracker;
use tidb_sessionctx::distsql_ctx::DistSQLContext;
use tidb_sessionctx::errctx::StrictNoWarningContext;
use tidb_sessionctx::ranger::Range;
use tidb_sessionctx::variable::tidb_defs::DefDistSQLScanConcurrency;
use tidb_tablecodec::tablecodec::{encode_row_key, verify_table_id_for_ranges};
use tidb_types::collate::get_binary_collator_slice;
use tidb_types::datum::{KindInt64, new_bytes_datum, new_uint_datum};

/// Go `placement.DCLabelKey` (`"zone"`).
const DCLabelKey: &str = "zone";

const estimatedRegionRowCount: u64 = 100000;

/// RequestBuilder is used to build a [`Request`].
/// It is called before we issue a kv request by [`crate::distsql::select`].
/// Notice a builder can only be used once unless it returns an error in test.
#[derive(Default)]
pub struct RequestBuilder {
    pub request: Request,
    is: Option<Arc<dyn MetaOnlyInfoSchema>>,
    err: Option<Error>,
    used: bool,
    /// When [`Self::set_dag_request`] is called, builder stores the DAG request here.
    dag: Option<tipb::DagRequest>,
}

/// Ensure all `(gogoproto.nullable) = false` fields on `DAGRequest` (fields 3, 4, 8, 11) and
/// its `ChunkMemoryLayout` / `Executor` items are marked present before `rust-protobuf 2.8`
/// serializes them, so zero values are emitted identically to Go's gogoproto (D07.3 spec §6.2).
// glue: proto2 gogoproto.nullable=false wire parity with Go tipb.DAGRequest.Marshal
fn normalize_dag_request_for_marshal(dag: &mut tipb::DagRequest) {
    if !dag.has_time_zone_offset() {
        dag.set_time_zone_offset(0);
    }
    if !dag.has_flags() {
        dag.set_flags(0);
    }
    if !dag.has_encode_type() {
        dag.set_encode_type(tipb::EncodeType::TypeDefault);
    }
    if !dag.has_time_zone_name() {
        dag.set_time_zone_name(String::new());
    }
    if dag.has_chunk_memory_layout() {
        let layout = dag.mut_chunk_memory_layout();
        if !layout.has_endian() {
            layout.set_endian(tipb::Endian::LittleEndian);
        }
    }
    for exec in dag.mut_executors().iter_mut() {
        if !exec.has_tp() {
            exec.set_tp(tipb::ExecType::TypeTableScan);
        }
        if !exec.has_fine_grained_shuffle_stream_count() {
            exec.set_fine_grained_shuffle_stream_count(0);
        }
        if !exec.has_fine_grained_shuffle_batch_size() {
            exec.set_fine_grained_shuffle_batch_size(0);
        }
    }
    for ch in dag.mut_intermediate_output_channels().iter_mut() {
        if !ch.has_executor_idx() {
            ch.set_executor_idx(0);
        }
    }
}

impl RequestBuilder {
    /// Build builds a [`Request`].
    ///
    /// Empty `key_ranges` is initialized before `verify_txn_scope` (and `verify_txn_scope`
    /// only runs when `self.err` is `None`) to avoid Go's nil-KeyRanges panic (D07.3 spec §8
    /// item 8). Single-use checking is active under `#[cfg(test)]` (Go `intest.InTest`).
    // go: RequestBuilder.Build L2 (initializes empty KeyRanges before verifyTxnScope; no failpoints)
    pub fn build(&mut self) -> Result<Request, Error> {
        #[cfg(test)]
        if self.used {
            return Err(Error::plain("request builder is already used"));
        }
        self.used = true;
        if self.request.read_replica_scope.is_empty() {
            self.request.read_replica_scope = GlobalReplicaScope.to_owned();
        }
        if self.request.replica_read.is_closest_read()
            && self.request.read_replica_scope != GlobalReplicaScope
        {
            self.request.match_store_labels = vec![StoreLabel {
                key: DCLabelKey.to_owned(),
                value: self.request.read_replica_scope.clone(),
            }];
        }
        if self.request.key_ranges.is_none() {
            self.request.key_ranges = Some(new_non_partitioned_key_ranges(Vec::new()));
        }
        // Go calls verifyTxnScope unconditionally and overwrites builder.err; Rust preserves
        // the first error (e.g. N10 runaway fence) rather than letting verify_txn_scope mask it.
        if self.err.is_none()
            && let Err(e) = self.verify_txn_scope()
        {
            self.err = Some(e);
        }

        if let Some(dag) = &self.dag {
            let execs = dag.get_executors();
            if execs.len() == 1 {
                // select * from t order by id
                if self.request.keep_order
                    && self.request.concurrency == DefDistSQLScanConcurrency as isize
                {
                    match execs[0].get_tp() {
                        tipb::ExecType::TypeTableScan
                        | tipb::ExecType::TypeIndexScan
                        | tipb::ExecType::TypePartitionTableScan => {
                            self.request.concurrency = 2;
                        }
                        _ => {}
                    }
                }
            }
        }

        if let Some(e) = self.err.clone() {
            return Err(e);
        }
        Ok(self.request.clone())
    }

    /// SetMemTracker sets a memTracker for this request.
    // go: RequestBuilder.SetMemTracker L1
    pub fn set_mem_tracker(&mut self, tracker: Option<Arc<Tracker>>) -> &mut Self {
        self.request.mem_tracker = tracker;
        self
    }

    /// SetTableRanges sets `KeyRanges` for `kv.Request` by converting `table_ranges` to
    /// `KeyRanges` firstly.
    // go: RequestBuilder.SetTableRanges L1
    pub fn set_table_ranges(&mut self, tid: i64, table_ranges: &[Range]) -> &mut Self {
        if self.err.is_none() {
            self.request.key_ranges = Some(new_non_partitioned_key_ranges(
                table_ranges_to_kv_ranges(tid, table_ranges),
            ));
        }
        self
    }

    /// SetHandleRanges sets `KeyRanges` for `kv.Request` by converting table handle range
    /// `ranges` to `KeyRanges` firstly.
    ///
    /// Unlike Go (`request_builder.go:158`), skips `SetToNonPartitioned()` when a prior
    /// error is already recorded (D07.3 spec §8 item 2, Go-API-only fix).
    // go: RequestBuilder.SetHandleRanges L2 (checks self.err before SetToNonPartitioned, spec §8 item 2)
    pub fn set_handle_ranges(
        &mut self,
        dctx: Option<&DistSQLContext>,
        tid: i64,
        is_common_handle: bool,
        ranges: &[Range],
    ) -> &mut Self {
        self.set_handle_ranges_for_tables(dctx, &[tid], is_common_handle, ranges);
        if self.err.is_none()
            && let Some(krs) = &mut self.request.key_ranges
            && let Err(e) = krs.set_to_non_partitioned()
        {
            self.err = Some(e);
        }
        self
    }

    /// SetHandleRangesForTables sets `KeyRanges` for `kv.Request` by converting table handle
    /// range `ranges` to `KeyRanges` firstly for multiple tables.
    // go: RequestBuilder.SetHandleRangesForTables L1
    pub fn set_handle_ranges_for_tables(
        &mut self,
        dctx: Option<&DistSQLContext>,
        tids: &[i64],
        is_common_handle: bool,
        ranges: &[Range],
    ) -> &mut Self {
        if self.err.is_none() {
            match table_handle_ranges_to_kv_ranges(dctx, tids, is_common_handle, ranges) {
                Ok(krs) => self.request.key_ranges = Some(krs),
                Err(e) => self.err = Some(e),
            }
        }
        self
    }

    /// SetDAGRequest sets the request type to `ReqTypeDAG` and constructs request data.
    // go: RequestBuilder.SetDAGRequest L1
    pub fn set_dag_request(&mut self, mut dag: tipb::DagRequest) -> &mut Self {
        if self.err.is_none() {
            self.request.tp = ReqTypeDAG;
            self.request.cacheable = true;
            normalize_dag_request_for_marshal(&mut dag);
            match dag.write_to_bytes() {
                Ok(data) => self.request.data = data,
                Err(e) => self.err = Some(Error::plain(e.to_string())),
            }
            let execs = dag.get_executors();
            let exec_cnt = execs.len();
            if exec_cnt != 0 && execs[exec_cnt - 1].has_limit() {
                self.request.limit_size = execs[exec_cnt - 1].get_limit().get_limit();
            }

            if exec_cnt >= 2 {
                // When the DAG is just a simple scan and small limit, set concurrency to 1
                // (or partition count) would be sufficient.
                let second_exec = &execs[1];
                if second_exec.has_limit()
                    && second_exec.get_limit().get_limit() < estimatedRegionRowCount
                {
                    let mut minimal_concurrency = false;
                    if exec_cnt == 2 {
                        // execCnt == 2 indicates TableScan / IndexScan -> Limit
                        minimal_concurrency = true;
                    } else {
                        // if execCnt > 2 and the limit's parent is IndexLookup,
                        // it means the DAG is a push-down IndexLookUp and its build side is
                        // IndexScan -> Limit.
                        let limit_parent = second_exec.get_parent_idx() as usize;
                        if limit_parent > 0
                            && limit_parent < exec_cnt
                            && execs[limit_parent].has_index_lookup()
                        {
                            minimal_concurrency = true;
                        }
                    }

                    if minimal_concurrency {
                        if let Some(kr) = &self.request.key_ranges {
                            self.request.concurrency = kr.partition_num();
                        } else {
                            self.request.concurrency = 1;
                        }
                    }
                }
            }
            self.dag = Some(dag);
        }
        self
    }

    /// SetKeyRanges sets `KeyRanges` for `kv.Request`.
    // go: RequestBuilder.SetKeyRanges L1
    pub fn set_key_ranges(&mut self, key_ranges: Vec<KeyRange>) -> &mut Self {
        self.request.key_ranges = Some(new_non_partitioned_key_ranges(key_ranges));
        self
    }

    /// SetKeyRangesWithHints sets `KeyRanges` for `kv.Request` with row count hints.
    // go: RequestBuilder.SetKeyRangesWithHints L1
    pub fn set_key_ranges_with_hints(
        &mut self,
        key_ranges: Vec<KeyRange>,
        hints: Option<Vec<isize>>,
    ) -> &mut Self {
        self.request.key_ranges = Some(new_non_paritioned_key_ranges_with_hint(key_ranges, hints));
        self
    }

    /// SetPartitionKeyRanges sets the `KeyRanges` for `kv.Request` on partitioned table cases.
    // go: RequestBuilder.SetPartitionKeyRanges L1
    pub fn set_partition_key_ranges(&mut self, key_ranges: Vec<Vec<KeyRange>>) -> &mut Self {
        self.request.key_ranges = Some(new_partitioned_key_ranges(key_ranges));
        self
    }

    /// SetStartTS sets `StartTS` for `kv.Request`.
    // go: RequestBuilder.SetStartTS L1
    pub fn set_start_ts(&mut self, start_ts: u64) -> &mut Self {
        self.request.start_ts = start_ts;
        self
    }

    /// SetDesc sets `Desc` for `kv.Request`.
    // go: RequestBuilder.SetDesc L1
    pub fn set_desc(&mut self, desc: bool) -> &mut Self {
        self.request.desc = desc;
        self
    }

    /// SetKeepOrder sets `KeepOrder` for `kv.Request`.
    // go: RequestBuilder.SetKeepOrder L1
    pub fn set_keep_order(&mut self, order: bool) -> &mut Self {
        self.request.keep_order = order;
        self
    }

    /// SetStoreType sets `StoreType` for `kv.Request`.
    // go: RequestBuilder.SetStoreType L1
    pub fn set_store_type(&mut self, store_type: StoreType) -> &mut Self {
        self.request.store_type = store_type;
        self
    }

    /// SetAllowBatchCop sets `BatchCop` property.
    // go: RequestBuilder.SetAllowBatchCop L1
    pub fn set_allow_batch_cop(&mut self, batch_cop: bool) -> &mut Self {
        self.request.batch_cop = batch_cop;
        self
    }

    // go: RequestBuilder.getIsolationLevel L1
    fn get_isolation_level(&self) -> IsoLevel {
        if self.request.tp == ReqTypeAnalyze {
            return RC;
        }
        SI
    }

    // go: RequestBuilder.getKVPriority L1
    fn get_kv_priority(&self, dctx: &DistSQLContext) -> isize {
        match dctx.priority {
            NoPriority | DelayedPriority => PriorityNormal,
            LowPriority => PriorityLow,
            HighPriority => PriorityHigh,
            _ => PriorityNormal,
        }
    }

    /// SetFromSessionVars sets the following fields for `kv.Request` from session variables:
    /// `Concurrency`, `IsolationLevel`, `NotFillCache`, `TaskID`, `Priority`, `ReplicaRead`,
    /// `ResourceGroupTagger`, `ResourceGroupName`, `Paging`, `RequestSource`, `StoreBatchSize`,
    /// `StoreBusyThreshold`, `TiKVClientReadTimeout`.
    ///
    /// When `dctx.has_runaway_rule` is true, records `blocked("N10", ...)` so `build()`
    /// fails explicitly rather than silently ignoring runaway rules (D07.3 spec §5).
    // go: RequestBuilder.SetFromSessionVars L2 (has_runaway_rule sets blocked-by:N10)
    pub fn set_from_session_vars(&mut self, dctx: &DistSQLContext) -> &mut Self {
        let distsql_concurrency = dctx.dist_sql_concurrency;
        if self.request.concurrency == 0 {
            // Concurrency unset.
            self.request.concurrency = distsql_concurrency;
        } else if self.request.concurrency > distsql_concurrency {
            // Concurrency is set in SetDAGRequest, check the upper limit.
            self.request.concurrency = distsql_concurrency;
        }
        let mut replica_read_type = dctx.replica_read_type;
        if dctx.weak_consistency {
            self.request.isolation_level = RC;
        } else if dctx.rc_check_ts {
            self.request.isolation_level = RCCheckTS;
            replica_read_type = ReplicaReadLeader;
        } else {
            self.request.isolation_level = self.get_isolation_level();
        }
        self.request.not_fill_cache = dctx.not_fill_cache;
        self.request.task_id = dctx.task_id;
        self.request.priority = self.get_kv_priority(dctx);
        self.request.replica_read = replica_read_type;
        self.set_resource_group_tagger(dctx.resource_group_tagger.clone());
        {
            self.set_paging(dctx.enable_paging);
            self.request.paging.min_paging_size = dctx.min_paging_size as u64;
            self.request.paging.max_paging_size = dctx.max_paging_size as u64;
        }
        self.request.request_source.request_source_internal = dctx.in_restricted_sql;
        self.request.request_source.request_source_type = dctx.request_source_type.clone();
        self.request.request_source.explicit_request_source_type =
            dctx.explicit_request_source_type.clone();
        self.request.store_batch_size = dctx.store_batch_size;
        self.request.resource_group_name = dctx.resource_group_name.clone();
        self.request.store_busy_threshold = dctx.load_based_replica_read_threshold;
        if dctx.has_runaway_rule && self.err.is_none() {
            self.err = Some(blocked(
                "N10",
                "resource group runaway rule enforcement in distsql",
            ));
        }
        self.request.tikv_client_read_timeout = dctx.tikv_client_read_timeout;
        self
    }

    /// SetPaging sets `Paging` flag for `kv.Request`.
    // go: RequestBuilder.SetPaging L1
    pub fn set_paging(&mut self, paging: bool) -> &mut Self {
        self.request.paging.enable = paging;
        self
    }

    /// SetConcurrency sets `Concurrency` for `kv.Request`.
    // go: RequestBuilder.SetConcurrency L1
    pub fn set_concurrency(&mut self, concurrency: isize) -> &mut Self {
        self.request.concurrency = concurrency;
        self
    }

    /// SetCoprRequestRateLimit sets a shared in-flight cop request limiter for this request.
    // go: RequestBuilder.SetCoprRequestRateLimit L1
    pub fn set_copr_request_rate_limit(&mut self, rate_limit: Option<Arc<RateLimit>>) -> &mut Self {
        self.request.copr_request_rate_limit = rate_limit;
        self
    }

    /// SetTiDBServerID sets `TiDBServerID` for `kv.Request`.
    // go: RequestBuilder.SetTiDBServerID L1
    pub fn set_tidb_server_id(&mut self, server_id: u64) -> &mut Self {
        self.request.tidb_server_id = server_id;
        self
    }

    /// SetFromInfoSchema sets `is` and `SchemaVar` from infoSchema.
    // go: RequestBuilder.SetFromInfoSchema L1
    pub fn set_from_info_schema(&mut self, is: Arc<dyn MetaOnlyInfoSchema>) -> &mut Self {
        self.request.schema_var = is.schema_meta_version();
        self.is = Some(is);
        self
    }

    /// SetResourceGroupTagger sets the request resource group tagger.
    // go: RequestBuilder.SetResourceGroupTagger L1
    pub fn set_resource_group_tagger(
        &mut self,
        tagger: Option<Arc<ResourceGroupTagBuilder>>,
    ) -> &mut Self {
        self.request.resource_group_tagger = tagger;
        self
    }

    /// SetResourceGroupName sets the request resource group name.
    // go: RequestBuilder.SetResourceGroupName L1
    pub fn set_resource_group_name(&mut self, name: impl Into<String>) -> &mut Self {
        self.request.resource_group_name = name.into();
        self
    }

    /// SetExplicitRequestSourceType sets the explicit request source type.
    // go: RequestBuilder.SetExplicitRequestSourceType L1
    pub fn set_explicit_request_source_type(
        &mut self,
        source_type: impl Into<String>,
    ) -> &mut Self {
        self.request.request_source.explicit_request_source_type = source_type.into();
        self
    }

    // go: RequestBuilder.verifyTxnScope L2 (deterministic BTreeSet order; placement bundle hit returns blocked-by:X via verify_txn_scope)
    fn verify_txn_scope(&self) -> Result<(), Error> {
        let txn_scope = &self.request.txn_scope;
        if txn_scope.is_empty() || txn_scope == GlobalReplicaScope {
            return Ok(());
        }
        let Some(is) = &self.is else {
            return Ok(());
        };
        let Some(krs) = &self.request.key_ranges else {
            return Ok(());
        };
        let (tids, err) = verify_table_id_for_ranges(krs);
        err?;
        let visit_physical_table_id: BTreeSet<i64> = tids.into_iter().collect();
        for phy_table_id in visit_physical_table_id {
            let valid = verify_txn_scope(txn_scope, phy_table_id, is.as_ref())?;
            if !valid {
                return Err(Error::plain(format!(
                    "table {phy_table_id} can not be read by {txn_scope} txn_scope"
                )));
            }
        }
        Ok(())
    }

    /// SetTxnScope sets request TxnScope.
    // go: RequestBuilder.SetTxnScope L1
    pub fn set_txn_scope(&mut self, scope: impl Into<String>) -> &mut Self {
        self.request.txn_scope = scope.into();
        self
    }

    /// SetReadReplicaScope sets request readReplicaScope.
    // go: RequestBuilder.SetReadReplicaScope L1
    pub fn set_read_replica_scope(&mut self, scope: impl Into<String>) -> &mut Self {
        self.request.read_replica_scope = scope.into();
        self
    }

    /// SetIsStaleness sets request IsStaleness.
    // go: RequestBuilder.SetIsStaleness L1
    pub fn set_is_staleness(&mut self, is: bool) -> &mut Self {
        self.request.is_staleness = is;
        self
    }

    /// SetClosestReplicaReadAdjuster sets request CoprRequestAdjuster.
    // go: RequestBuilder.SetClosestReplicaReadAdjuster L1
    pub fn set_closest_replica_read_adjuster(
        &mut self,
        chk_fn: Option<CoprRequestAdjuster>,
    ) -> &mut Self {
        self.request.closest_replica_read_adjuster = chk_fn;
        self
    }

    /// SetConnIDAndConnAlias sets connection id and alias for the builder.
    // go: RequestBuilder.SetConnIDAndConnAlias L1
    pub fn set_conn_id_and_conn_alias(
        &mut self,
        conn_id: u64,
        conn_alias: impl Into<String>,
    ) -> &mut Self {
        self.request.conn_id = conn_id;
        self.request.conn_alias = conn_alias.into();
        self
    }
}

/// TableHandleRangesToKVRanges converts table handle ranges to [`KeyRanges`] for multiple tables.
// go: TableHandleRangesToKVRanges L1
pub fn table_handle_ranges_to_kv_ranges(
    dctx: Option<&DistSQLContext>,
    tids: &[i64],
    is_common_handle: bool,
    ranges: &[Range],
) -> Result<KeyRanges, Error> {
    if !is_common_handle {
        return Ok(tables_ranges_to_kv_ranges(tids, ranges));
    }
    common_handle_ranges_to_kv_ranges(dctx, tids, ranges)
}

/// TableRangesToKVRanges converts table ranges to `Vec<KeyRange>`.
// go: TableRangesToKVRanges L1
pub fn table_ranges_to_kv_ranges(tid: i64, ranges: &[Range]) -> Vec<KeyRange> {
    if ranges.is_empty() {
        return Vec::new();
    }
    tables_ranges_to_kv_ranges(&[tid], ranges)
        .first_partition_range()
        .to_vec()
}

/// tablesRangesToKVRanges converts table ranges to [`KeyRanges`].
// go: tablesRangesToKVRanges L1
fn tables_ranges_to_kv_ranges(tids: &[i64], ranges: &[Range]) -> KeyRanges {
    table_ranges_to_kv_ranges_without_split(tids, ranges)
}

// go: tableRangesToKVRangesWithoutSplit L1
fn table_ranges_to_kv_ranges_without_split(tids: &[i64], ranges: &[Range]) -> KeyRanges {
    let mut krs: Vec<Vec<KeyRange>> = (0..tids.len())
        .map(|_| Vec::with_capacity(ranges.len()))
        .collect();
    for ran in ranges {
        let (low, high) = encode_handle_key(ran);
        for (i, &tid) in tids.iter().enumerate() {
            let start_key = encode_row_key(tid, &low);
            let end_key = encode_row_key(tid, &high);
            krs[i].push(KeyRange { start_key, end_key });
        }
    }
    new_partitioned_key_ranges(krs)
}

// go: encodeHandleKey L1
fn encode_handle_key(ran: &Range) -> (Vec<u8>, Vec<u8>) {
    let mut low = Vec::with_capacity(8);
    encode_int(&mut low, ran.low_val[0].get_int64());
    let mut high = Vec::with_capacity(8);
    encode_int(&mut high, ran.high_val[0].get_int64());
    if ran.low_exclude {
        low = Key(low).prefix_next().0;
    }
    if !ran.high_exclude {
        high = Key(high).prefix_next().0;
    }
    (low, high)
}

/// SplitRangesAcrossInt64Boundary splits the ranges into two groups:
/// 1. `signed_ranges`: `<= i64::MAX`
/// 2. `unsigned_ranges`: `> i64::MAX`
///
/// If `keep_order` is false, merges `(unsigned_ranges ++ signed_ranges, empty)` to save one
/// RPC. If `keep_order` is true, returns `(signed, unsigned)` when `!desc` and
/// `(unsigned, signed)` when `desc`.
// go: SplitRangesAcrossInt64Boundary L1
pub fn split_ranges_across_int64_boundary(
    ranges: &[Range],
    keep_order: bool,
    desc: bool,
    is_common_handle: bool,
) -> (Vec<Range>, Vec<Range>) {
    if is_common_handle || ranges.is_empty() || ranges[0].low_val[0].kind() == KindInt64 {
        return (ranges.to_vec(), Vec::new());
    }
    let max_i64 = i64::MAX as u64;
    let idx = sort::search(ranges.len(), |i| {
        ranges[i].high_val[0].get_uint64() > max_i64
    });
    if idx == ranges.len() {
        return (ranges.to_vec(), Vec::new());
    }
    if ranges[idx].low_val[0].get_uint64() > max_i64 {
        let signed_ranges = &ranges[0..idx];
        let unsigned_ranges = &ranges[idx..];
        if !keep_order {
            let mut merged = Vec::with_capacity(unsigned_ranges.len() + signed_ranges.len());
            merged.extend_from_slice(unsigned_ranges);
            merged.extend_from_slice(signed_ranges);
            return (merged, Vec::new());
        }
        if desc {
            return (unsigned_ranges.to_vec(), signed_ranges.to_vec());
        }
        return (signed_ranges.to_vec(), unsigned_ranges.to_vec());
    }
    // need to split the range that straddles the int64 boundary
    let mut signed_ranges = Vec::with_capacity(idx + 1);
    let mut unsigned_ranges = Vec::with_capacity(ranges.len() - idx);
    signed_ranges.extend_from_slice(&ranges[0..idx]);
    if !(ranges[idx].low_val[0].get_uint64() == max_i64 && ranges[idx].low_exclude) {
        signed_ranges.push(Range {
            low_val: ranges[idx].low_val.clone(),
            low_exclude: ranges[idx].low_exclude,
            high_val: vec![new_uint_datum(max_i64)],
            high_exclude: false,
            collators: ranges[idx].collators.clone(),
        });
    }
    if !(ranges[idx].high_val[0].get_uint64() == max_i64 + 1 && ranges[idx].high_exclude) {
        unsigned_ranges.push(Range {
            low_val: vec![new_uint_datum(max_i64 + 1)],
            low_exclude: false,
            high_val: ranges[idx].high_val.clone(),
            high_exclude: ranges[idx].high_exclude,
            collators: ranges[idx].collators.clone(),
        });
    }
    if idx < ranges.len() {
        unsigned_ranges.extend_from_slice(&ranges[idx + 1..]);
    }
    if !keep_order {
        unsigned_ranges.extend(signed_ranges);
        return (unsigned_ranges, Vec::new());
    }
    if desc {
        return (unsigned_ranges, signed_ranges);
    }
    (signed_ranges, unsigned_ranges)
}

/// CommonHandleRangesToKVRanges converts common handle ranges to [`KeyRanges`].
// go: CommonHandleRangesToKVRanges L1
pub fn common_handle_ranges_to_kv_ranges(
    dctx: Option<&DistSQLContext>,
    tids: &[i64],
    ranges: &[Range],
) -> Result<KeyRanges, Error> {
    let mut rans = Vec::with_capacity(ranges.len());
    for ran in ranges {
        let (low, high) = encode_index_key(dctx, ran)?;
        rans.push(Range {
            low_val: vec![new_bytes_datum(low)],
            high_val: vec![new_bytes_datum(high)],
            low_exclude: false,
            high_exclude: true,
            collators: get_binary_collator_slice(1),
        });
    }
    let mut krs: Vec<Vec<KeyRange>> = (0..tids.len())
        .map(|_| Vec::with_capacity(ranges.len()))
        .collect();
    for ran in &mut rans {
        let mut low = ran.low_val[0].get_bytes().to_vec();
        let high = ran.high_val[0].get_bytes().to_vec();
        if ran.low_exclude {
            low = Key(low).prefix_next().0;
        }
        ran.low_val[0].set_bytes(low.clone());
        for (i, &tid) in tids.iter().enumerate() {
            let start_key = encode_row_key(tid, &low);
            let end_key = encode_row_key(tid, &high);
            krs[i].push(KeyRange { start_key, end_key });
        }
    }
    Ok(new_partitioned_key_ranges(krs))
}

/// VerifyTxnScope verifies whether `txn_scope` and visited physical table break the leader
/// rule's dcLocation.
///
/// L2: returns `Result<bool, Error>` so a non-global txn scope hitting a table with a
/// placement bundle stops explicitly with `blocked("X", ...)` (placement rule parsing and
/// local TSO are X; D07.3 spec §4.9, §5).
// go: VerifyTxnScope L2 (returns Result<bool, Error>; non-global scope with placement bundle returns blocked-by:X)
pub fn verify_txn_scope(
    txn_scope: &str,
    physical_table_id: i64,
    is: &dyn MetaOnlyInfoSchema,
) -> Result<bool, Error> {
    if txn_scope.is_empty() || txn_scope == GlobalTxnScope {
        return Ok(true);
    }
    if is
        .placement_bundle_by_physical_table_id(physical_table_id)
        .is_some()
    {
        return Err(blocked(
            "X",
            "placement bundle leader DC check for non-global txn_scope",
        ));
    }
    Ok(true)
}

/// EncodeIndexKey gets encoded keys containing low and high.
// go: EncodeIndexKey L1
pub fn encode_index_key(
    dctx: Option<&DistSQLContext>,
    ran: &Range,
) -> Result<(Vec<u8>, Vec<u8>), Error> {
    let (tz, err_ctx) = match dctx {
        Some(ctx) => (Arc::clone(&ctx.location), ctx.err_ctx.clone()),
        None => (utc(), StrictNoWarningContext.clone()),
    };

    let mut low = Vec::new();
    if let Err(e) = encode_key(&tz, &mut low, &ran.low_val) {
        err_ctx.handle_error(e)?;
    }
    if ran.low_exclude {
        low = Key(low).prefix_next().0;
    }

    let mut high = Vec::new();
    if let Err(e) = encode_key(&tz, &mut high, &ran.high_val) {
        err_ctx.handle_error(e)?;
    }
    if !ran.high_exclude {
        high = Key(high).prefix_next().0;
    }
    Ok((low, high))
}
