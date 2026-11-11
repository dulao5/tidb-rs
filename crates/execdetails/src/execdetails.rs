//! go-file: pkg/util/execdetails/execdetails.go
//!
//! Execution details: the per-statement summary of coprocessor / commit / lock details
//! (`ExecDetails`, `SyncExecDetails`, P90 summaries) printed in the slow log and the
//! statement summary, and the per-plan runtime statistics (`RuntimeStatsColl`,
//! `BasicRuntimeStats`, `CopRuntimeStats`, ...) printed by `EXPLAIN ANALYZE`.
//!
//! Mapping:
//! - `time.Duration` is `i64` nanoseconds ([`Duration`]); Go's integer additions wrap, so do
//!   the port's (`wrapping_add`).
//! - Go pointers that one owner stores and later merges into (`SyncExecDetails` keeps the
//!   first `*util.CommitDetails` it is given and `Merge`s later ones into it;
//!   `RuntimeStatsWithCommit.MergeCommitDetails` and `RuntimeStatsColl.RegisterStats` do the
//!   same with the caller's object) are owned values here: the port stores a copy / takes
//!   the registered stats by value, so later merges do not write through to the caller's
//!   object (block spec §4).
//! - The `RuntimeStats` interface is [`RuntimeStats`]; Go's type assertions in `Merge`
//!   (`rs.(*BasicRuntimeStats)`) are `as_any().downcast_ref`.
//! - `sync.Mutex` fields: [`SyncExecDetails`] and [`RuntimeStatsColl`] hold a
//!   [`std::sync::Mutex`] (M-SYNC); [`RootRuntimeStats`] (mutated only under the
//!   collection's lock in Go, read without it) has its own mutex; [`CopRuntimeStats`] is
//!   handed out as `Arc<Mutex<..>>` (its `String` sorts / processes the percentile in place,
//!   as Go's does through the shared slice and t-digest pointer).
//! - `[]zap.Field` is `Vec<`[`LogField`]`>`.
//! - Maps whose iteration order Go randomizes only feed order-independent results; the port
//!   uses `BTreeMap`.

use std::any::Any;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicI32, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use tidb_base::gostd::conv::f64_to_i64;
use tidb_base::gostd::sort;
use tidb_base::gostd::strconv::format_float;
use tidb_base::gostd::time::{
    Duration, MICROSECOND, MILLISECOND, NANOSECOND, SECOND, duration_string,
};
use tidb_base::kv::{self, StoreType};
use tipb::ExecutorExecutionSummary;

use crate::clientgo::{CommitDetails, LockKeysDetails, RUDetails, ScanDetail, TimeDetail};
use crate::tdigest::TDigest;

/// A `zap.Field` of the slow-query log line (`ToZapFields`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogField {
    pub key: String,
    pub value: LogValue,
}

/// The value of a [`LogField`]: `zap.String` or `zap.Int` / `zap.Int32`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LogValue {
    String(String),
    Int(i64),
}

// glue: zap.String
fn zap_string(key: &str, v: String) -> LogField {
    LogField {
        key: key.to_string(),
        value: LogValue::String(v),
    }
}

// glue: zap.Int / zap.Int32
fn zap_int(key: &str, v: i64) -> LogField {
    LogField {
        key: key.to_string(),
        value: LogValue::Int(v),
    }
}

/// `strconv.FormatFloat(d.Seconds(), 'f', prec, 64)`.
// glue: time.Duration.Seconds formatted with strconv.FormatFloat
fn secs(d: Duration, prec: isize) -> String {
    format_float(duration_seconds(d), b'f', prec, 64)
}

/// `time.Duration.Seconds`.
// go-std: time.Duration.Seconds L1
fn duration_seconds(d: Duration) -> f64 {
    let sec = d / SECOND;
    let nsec = d % SECOND;
    sec as f64 + nsec as f64 / 1e9
}

/// `fmt.Sprintf("%v", []string{...})`: `[a b c]`.
// go-std: fmt %v of []string L1
fn sprint_strings(s: &[String]) -> String {
    format!("[{}]", s.join(" "))
}

/// `ExecDetails` contains execution detail information.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ExecDetails {
    /// The embedded `DetailsNeedP90` (its fields are promoted in Go).
    pub details_need_p90: DetailsNeedP90,
    pub commit_detail: Option<Box<CommitDetails>>,
    pub lock_keys_detail: Option<Box<LockKeysDetails>>,
    pub shared_lock_keys_detail: Option<Box<LockKeysDetails>>,
    pub scan_detail: Option<Box<ScanDetail>>,
    pub cop_time: Duration,
    pub backoff_time: Duration,
    pub lock_keys_duration: Duration,
    pub request_count: i64,
}

/// `DetailsNeedP90` contains execution detail information which need calculate P90.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DetailsNeedP90 {
    pub backoff_sleep: BTreeMap<String, Duration>,
    pub backoff_times: BTreeMap<String, i64>,
    pub callee_address: String,
    pub time_detail: TimeDetail,
}

/// `P90BackoffSummary` contains execution summary for a backoff type.
#[derive(Clone, Debug, Default)]
pub struct P90BackoffSummary {
    pub req_times: i64,
    pub backoff_percentile: Percentile<DurationWithAddr>,
    pub tot_backoff_time: Duration,
    pub tot_backoff_times: i64,
}

/// `P90Summary` contains execution summary for cop tasks. Go's nil `BackoffInfo` map (a zero
/// summary, before the first `Merge` / `Reset`) is `None`.
#[derive(Clone, Debug, Default)]
pub struct P90Summary {
    pub num_cop_tasks: i64,
    pub process_time_percentile: Percentile<DurationWithAddr>,
    pub wait_time_percentile: Percentile<DurationWithAddr>,
    pub backoff_info: Option<BTreeMap<String, P90BackoffSummary>>,
}

/// `CopExecDetails` contains cop execution detail information.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CopExecDetails {
    pub scan_detail: ScanDetail,
    pub time_detail: TimeDetail,
    pub callee_address: String,
    pub backoff_time: Duration,
    pub backoff_sleep: BTreeMap<String, Duration>,
    pub backoff_times: BTreeMap<String, i64>,
}

/// `MaxDetailsNumsForOneQuery` is the max number of details to keep for P90 for one query.
pub const MaxDetailsNumsForOneQuery: usize = 1000;

impl P90Summary {
    /// `Reset` resets all fields in DetailsNeedP90Summary.
    // go: P90Summary.Reset L1
    pub fn reset(&mut self) {
        self.num_cop_tasks = 0;
        self.process_time_percentile = Percentile::default();
        self.wait_time_percentile = Percentile::default();
        self.backoff_info = Some(BTreeMap::new());
    }

    /// `Merge` merges DetailsNeedP90 into P90Summary.
    // go: P90Summary.Merge L1
    pub fn merge(&mut self, detail: &DetailsNeedP90) {
        if self.backoff_info.is_none() {
            self.reset();
        }
        self.num_cop_tasks = self.num_cop_tasks.wrapping_add(1);
        self.process_time_percentile.add(DurationWithAddr {
            d: detail.time_detail.process_time,
            addr: detail.callee_address.clone(),
        });
        self.wait_time_percentile.add(DurationWithAddr {
            d: detail.time_detail.wait_time,
            addr: detail.callee_address.clone(),
        });

        let info_map = self.backoff_info.as_mut().expect("reset above");
        for (backoff, &time_item) in &detail.backoff_times {
            let info = info_map.entry(backoff.clone()).or_default();
            let sleep_item = detail.backoff_sleep.get(backoff).copied().unwrap_or(0);
            info.req_times = info.req_times.wrapping_add(1);
            info.tot_backoff_time = info.tot_backoff_time.wrapping_add(sleep_item);
            info.tot_backoff_times = info.tot_backoff_times.wrapping_add(time_item);

            info.backoff_percentile.add(DurationWithAddr {
                d: sleep_item,
                addr: detail.callee_address.clone(),
            });
        }
    }
}

/// `StmtExecDetails` contains stmt level execution detail info.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StmtExecDetails {
    pub write_sql_resp_duration: Duration,
}

/// CopTimeStr represents the sum of cop-task time spend in TiDB distSQL.
pub const CopTimeStr: &str = "Cop_time";
/// WaitTimeStr means the time of all coprocessor wait.
pub const WaitTimeStr: &str = "Wait_time";
/// LockKeysTimeStr means the time interval between pessimistic lock wait start and lock got obtain
pub const LockKeysTimeStr: &str = "LockKeys_time";
/// RequestCountStr means the request count.
pub const RequestCountStr: &str = "Request_count";
/// WaitPrewriteBinlogTimeStr means the time of waiting prewrite binlog finished when transaction committing.
pub const WaitPrewriteBinlogTimeStr: &str = "Wait_prewrite_binlog_time";
/// GetCommitTSTimeStr means the time of getting commit ts.
pub const GetCommitTSTimeStr: &str = "Get_commit_ts_time";
/// GetLatestTsTimeStr means the time of getting latest ts in async commit and 1pc.
pub const GetLatestTsTimeStr: &str = "Get_latest_ts_time";
/// CommitBackoffTimeStr means the time of commit backoff.
pub const CommitBackoffTimeStr: &str = "Commit_backoff_time";
/// BackoffTypesStr means the backoff type.
pub const BackoffTypesStr: &str = "Backoff_types";
/// SlowestPrewriteRPCDetailStr means the details of the slowest RPC during the transaction 2pc prewrite process.
pub const SlowestPrewriteRPCDetailStr: &str = "Slowest_prewrite_rpc_detail";
/// CommitPrimaryRPCDetailStr means the details of the slowest RPC during the transaction 2pc commit process.
pub const CommitPrimaryRPCDetailStr: &str = "Commit_primary_rpc_detail";
/// ResolveLockTimeStr means the time of resolving lock.
pub const ResolveLockTimeStr: &str = "Resolve_lock_time";
/// LocalLatchWaitTimeStr means the time of waiting in local latch.
pub const LocalLatchWaitTimeStr: &str = "Local_latch_wait_time";
/// TxnRetryStr means the count of transaction retry.
pub const TxnRetryStr: &str = "Txn_retry";
/// GetSnapshotTimeStr means the time spent on getting an engine snapshot.
pub const GetSnapshotTimeStr: &str = "Get_snapshot_time";
/// RocksdbDeleteSkippedCountStr means the count of rocksdb delete skipped count.
pub const RocksdbDeleteSkippedCountStr: &str = "Rocksdb_delete_skipped_count";
/// RocksdbKeySkippedCountStr means the count of rocksdb key skipped count.
pub const RocksdbKeySkippedCountStr: &str = "Rocksdb_key_skipped_count";
/// RocksdbBlockCacheHitCountStr means the count of rocksdb block cache hit.
pub const RocksdbBlockCacheHitCountStr: &str = "Rocksdb_block_cache_hit_count";
/// RocksdbBlockReadCountStr means the count of rocksdb block read.
pub const RocksdbBlockReadCountStr: &str = "Rocksdb_block_read_count";
/// RocksdbBlockReadByteStr means the bytes of rocksdb block read.
pub const RocksdbBlockReadByteStr: &str = "Rocksdb_block_read_byte";
/// RocksdbBlockReadTimeStr means the time spent on rocksdb block read.
pub const RocksdbBlockReadTimeStr: &str = "Rocksdb_block_read_time";
/// ProcessTimeStr represents the sum of process time of all the coprocessor tasks.
pub const ProcessTimeStr: &str = "Process_time";
/// BackoffTimeStr means the time of all back-off.
pub const BackoffTimeStr: &str = "Backoff_time";
/// TotalKeysStr means the total scan keys.
pub const TotalKeysStr: &str = "Total_keys";
/// ProcessKeysStr means the total processed keys.
pub const ProcessKeysStr: &str = "Process_keys";
/// PreWriteTimeStr means the time of pre-write.
pub const PreWriteTimeStr: &str = "Prewrite_time";
/// CommitTimeStr means the time of commit.
pub const CommitTimeStr: &str = "Commit_time";
/// WriteKeysStr means the count of keys in the transaction.
pub const WriteKeysStr: &str = "Write_keys";
/// WriteSizeStr means the key/value size in the transaction.
pub const WriteSizeStr: &str = "Write_size";
/// PrewriteRegionStr means the count of region when pre-write.
pub const PrewriteRegionStr: &str = "Prewrite_region";

impl ExecDetails {
    /// `String` implements the fmt.Stringer interface.
    // go: ExecDetails.String L1
    pub fn string(&self) -> String {
        let mut parts: Vec<String> = Vec::with_capacity(8);
        let d = self;
        if d.cop_time > 0 {
            parts.push(format!("{CopTimeStr}: {}", secs(d.cop_time, -1)));
        }
        if d.details_need_p90.time_detail.process_time > 0 {
            parts.push(format!(
                "{ProcessTimeStr}: {}",
                secs(d.details_need_p90.time_detail.process_time, -1)
            ));
        }
        if d.details_need_p90.time_detail.wait_time > 0 {
            parts.push(format!(
                "{WaitTimeStr}: {}",
                secs(d.details_need_p90.time_detail.wait_time, -1)
            ));
        }
        if d.backoff_time > 0 {
            parts.push(format!("{BackoffTimeStr}: {}", secs(d.backoff_time, -1)));
        }
        if d.lock_keys_duration > 0 {
            parts.push(format!(
                "{LockKeysTimeStr}: {}",
                secs(d.lock_keys_duration, -1)
            ));
        }
        if d.request_count > 0 {
            parts.push(format!("{RequestCountStr}: {}", d.request_count));
        }
        if let Some(cd) = &d.commit_detail {
            if cd.prewrite_time > 0 {
                parts.push(format!("{PreWriteTimeStr}: {}", secs(cd.prewrite_time, -1)));
            }
            if cd.wait_prewrite_binlog_time > 0 {
                parts.push(format!(
                    "{WaitPrewriteBinlogTimeStr}: {}",
                    secs(cd.wait_prewrite_binlog_time, -1)
                ));
            }
            if cd.commit_time > 0 {
                parts.push(format!("{CommitTimeStr}: {}", secs(cd.commit_time, -1)));
            }
            if cd.get_commit_ts_time > 0 {
                parts.push(format!(
                    "{GetCommitTSTimeStr}: {}",
                    secs(cd.get_commit_ts_time, -1)
                ));
            }
            if cd.get_latest_ts_time > 0 {
                parts.push(format!(
                    "{GetLatestTsTimeStr}: {}",
                    secs(cd.get_latest_ts_time, -1)
                ));
            }
            let commit_backoff_time = cd.mu.commit_backoff_time;
            if commit_backoff_time > 0 {
                parts.push(format!(
                    "{CommitBackoffTimeStr}: {}",
                    secs(commit_backoff_time, -1)
                ));
            }
            if !cd.mu.prewrite_backoff_types.is_empty() {
                parts.push(format!(
                    "Prewrite_{BackoffTypesStr}: {}",
                    sprint_strings(&cd.mu.prewrite_backoff_types)
                ));
            }
            if !cd.mu.commit_backoff_types.is_empty() {
                parts.push(format!(
                    "Commit_{BackoffTypesStr}: {}",
                    sprint_strings(&cd.mu.commit_backoff_types)
                ));
            }
            let sp = &cd.mu.slowest_prewrite;
            if sp.req_total_time > 0 {
                parts.push(format!(
                    "{SlowestPrewriteRPCDetailStr}: {{total:{}s, region_id: {}, store: {}, {}}}",
                    secs(sp.req_total_time, 3),
                    sp.region,
                    sp.store_addr,
                    sp.exec_details.string()
                ));
            }
            let cp = &cd.mu.commit_primary;
            if cp.req_total_time > 0 {
                parts.push(format!(
                    "{CommitPrimaryRPCDetailStr}: {{total:{}s, region_id: {}, store: {}, {}}}",
                    secs(cp.req_total_time, 3),
                    cp.region,
                    cp.store_addr,
                    cp.exec_details.string()
                ));
            }
            let resolve_lock_time = cd.resolve_lock.resolve_lock_time;
            if resolve_lock_time > 0 {
                parts.push(format!(
                    "{ResolveLockTimeStr}: {}",
                    secs(resolve_lock_time, -1)
                ));
            }
            if cd.local_latch_time > 0 {
                parts.push(format!(
                    "{LocalLatchWaitTimeStr}: {}",
                    secs(cd.local_latch_time, -1)
                ));
            }
            if cd.write_keys > 0 {
                parts.push(format!("{WriteKeysStr}: {}", cd.write_keys));
            }
            if cd.write_size > 0 {
                parts.push(format!("{WriteSizeStr}: {}", cd.write_size));
            }
            let prewrite_region_num = cd.prewrite_region_num;
            if prewrite_region_num > 0 {
                parts.push(format!("{PrewriteRegionStr}: {prewrite_region_num}"));
            }
            if cd.txn_retry > 0 {
                parts.push(format!("{TxnRetryStr}: {}", cd.txn_retry));
            }
        }
        if let Some(sd) = &d.scan_detail {
            if sd.processed_keys > 0 {
                parts.push(format!("{ProcessKeysStr}: {}", sd.processed_keys));
            }
            if sd.total_keys > 0 {
                parts.push(format!("{TotalKeysStr}: {}", sd.total_keys));
            }
            if sd.get_snapshot_duration > 0 {
                parts.push(format!(
                    "{GetSnapshotTimeStr}: {}",
                    secs(sd.get_snapshot_duration, 3)
                ));
            }
            if sd.rocksdb_delete_skipped_count > 0 {
                parts.push(format!(
                    "{RocksdbDeleteSkippedCountStr}: {}",
                    sd.rocksdb_delete_skipped_count
                ));
            }
            if sd.rocksdb_key_skipped_count > 0 {
                parts.push(format!(
                    "{RocksdbKeySkippedCountStr}: {}",
                    sd.rocksdb_key_skipped_count
                ));
            }
            if sd.rocksdb_block_cache_hit_count > 0 {
                parts.push(format!(
                    "{RocksdbBlockCacheHitCountStr}: {}",
                    sd.rocksdb_block_cache_hit_count
                ));
            }
            if sd.rocksdb_block_read_count > 0 {
                parts.push(format!(
                    "{RocksdbBlockReadCountStr}: {}",
                    sd.rocksdb_block_read_count
                ));
            }
            if sd.rocksdb_block_read_byte > 0 {
                parts.push(format!(
                    "{RocksdbBlockReadByteStr}: {}",
                    sd.rocksdb_block_read_byte
                ));
            }
            if sd.rocksdb_block_read_duration > 0 {
                parts.push(format!(
                    "{RocksdbBlockReadTimeStr}: {}",
                    secs(sd.rocksdb_block_read_duration, 3)
                ));
            }
        }
        parts.join(" ")
    }

    /// `ToZapFields` wraps the ExecDetails as zap.Fields.
    // go: ExecDetails.ToZapFields L1
    pub fn to_zap_fields(&self) -> Vec<LogField> {
        let d = self;
        let mut fields = Vec::with_capacity(16);
        if d.cop_time > 0 {
            fields.push(zap_string(
                &CopTimeStr.to_lowercase(),
                secs(d.cop_time, -1) + "s",
            ));
        }
        if d.details_need_p90.time_detail.process_time > 0 {
            fields.push(zap_string(
                &ProcessTimeStr.to_lowercase(),
                secs(d.details_need_p90.time_detail.process_time, -1) + "s",
            ));
        }
        if d.details_need_p90.time_detail.wait_time > 0 {
            fields.push(zap_string(
                &WaitTimeStr.to_lowercase(),
                secs(d.details_need_p90.time_detail.wait_time, -1) + "s",
            ));
        }
        if d.backoff_time > 0 {
            fields.push(zap_string(
                &BackoffTimeStr.to_lowercase(),
                secs(d.backoff_time, -1) + "s",
            ));
        }
        if d.request_count > 0 {
            fields.push(zap_string(
                &RequestCountStr.to_lowercase(),
                d.request_count.to_string(),
            ));
        }
        if let Some(sd) = &d.scan_detail
            && sd.total_keys > 0
        {
            fields.push(zap_string(
                &TotalKeysStr.to_lowercase(),
                sd.total_keys.to_string(),
            ));
        }
        if let Some(sd) = &d.scan_detail
            && sd.processed_keys > 0
        {
            fields.push(zap_string(
                &ProcessKeysStr.to_lowercase(),
                sd.processed_keys.to_string(),
            ));
        }
        if let Some(cd) = &d.commit_detail {
            if cd.prewrite_time > 0 {
                fields.push(zap_string(
                    "prewrite_time",
                    secs(cd.prewrite_time, -1) + "s",
                ));
            }
            if cd.commit_time > 0 {
                fields.push(zap_string("commit_time", secs(cd.commit_time, -1) + "s"));
            }
            if cd.get_commit_ts_time > 0 {
                fields.push(zap_string(
                    "get_commit_ts_time",
                    secs(cd.get_commit_ts_time, -1) + "s",
                ));
            }
            let commit_backoff_time = cd.mu.commit_backoff_time;
            if commit_backoff_time > 0 {
                fields.push(zap_string(
                    "commit_backoff_time",
                    secs(commit_backoff_time, -1) + "s",
                ));
            }
            if !cd.mu.prewrite_backoff_types.is_empty() {
                fields.push(zap_string(
                    &format!("Prewrite_{BackoffTypesStr}"),
                    sprint_strings(&cd.mu.prewrite_backoff_types),
                ));
            }
            if !cd.mu.commit_backoff_types.is_empty() {
                fields.push(zap_string(
                    &format!("Commit_{BackoffTypesStr}"),
                    sprint_strings(&cd.mu.commit_backoff_types),
                ));
            }
            let sp = &cd.mu.slowest_prewrite;
            if sp.req_total_time > 0 {
                fields.push(zap_string(
                    SlowestPrewriteRPCDetailStr,
                    format!(
                        "total:{}s, region_id: {}, store: {}, {}}}",
                        secs(sp.req_total_time, 3),
                        sp.region,
                        sp.store_addr,
                        sp.exec_details.string()
                    ),
                ));
            }
            let cp = &cd.mu.commit_primary;
            if cp.req_total_time > 0 {
                fields.push(zap_string(
                    CommitPrimaryRPCDetailStr,
                    format!(
                        "{{total:{}s, region_id: {}, store: {}, {}}}",
                        secs(cp.req_total_time, 3),
                        cp.region,
                        cp.store_addr,
                        cp.exec_details.string()
                    ),
                ));
            }
            let resolve_lock_time = cd.resolve_lock.resolve_lock_time;
            if resolve_lock_time > 0 {
                fields.push(zap_string(
                    "resolve_lock_time",
                    secs(resolve_lock_time, -1) + "s",
                ));
            }
            if cd.local_latch_time > 0 {
                fields.push(zap_string(
                    "local_latch_wait_time",
                    secs(cd.local_latch_time, -1) + "s",
                ));
            }
            if cd.write_keys > 0 {
                fields.push(zap_int("write_keys", cd.write_keys));
            }
            if cd.write_size > 0 {
                fields.push(zap_int("write_size", cd.write_size));
            }
            let prewrite_region_num = cd.prewrite_region_num;
            if prewrite_region_num > 0 {
                fields.push(zap_int("prewrite_region", prewrite_region_num as i64));
            }
            if cd.txn_retry > 0 {
                fields.push(zap_int("txn_retry", cd.txn_retry));
            }
        }
        fields
    }
}

/// `SyncExecDetails` is a synced version of `ExecDetails` and its `P90Summary`.
#[derive(Debug, Default)]
pub struct SyncExecDetails {
    mu: Mutex<SyncExecDetailsInner>,
}

#[derive(Debug, Default)]
struct SyncExecDetailsInner {
    exec_details: ExecDetails,
    details_summary: P90Summary,
}

impl SyncExecDetails {
    // glue: lock the mutex (M-SYNC; a poisoned lock keeps the data, as Go's mutex has no poisoning)
    fn lock(&self) -> std::sync::MutexGuard<'_, SyncExecDetailsInner> {
        self.mu.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `MergeExecDetails` merges a single region execution details into self, used to print
    /// the information in slow query log. The first `commit_details` is stored as a copy
    /// (Go stores the pointer, block spec §4).
    // go: SyncExecDetails.MergeExecDetails L2 (commitDetails is stored by value)
    pub fn merge_exec_details(
        &self,
        details: Option<&ExecDetails>,
        commit_details: Option<&CommitDetails>,
    ) {
        let mut s = self.lock();
        if let Some(details) = details {
            s.exec_details.cop_time = s.exec_details.cop_time.wrapping_add(details.cop_time);
            s.exec_details.backoff_time = s
                .exec_details
                .backoff_time
                .wrapping_add(details.backoff_time);
            s.exec_details.request_count = s.exec_details.request_count.wrapping_add(1);
            s.merge_scan_detail(details.scan_detail.as_deref());
            s.merge_time_detail(details.details_need_p90.time_detail);
            let detail = DetailsNeedP90 {
                backoff_sleep: details.details_need_p90.backoff_sleep.clone(),
                backoff_times: details.details_need_p90.backoff_times.clone(),
                callee_address: details.details_need_p90.callee_address.clone(),
                time_detail: details.details_need_p90.time_detail,
            };
            s.details_summary.merge(&detail);
        }
        if let Some(commit_details) = commit_details {
            match &mut s.exec_details.commit_detail {
                None => s.exec_details.commit_detail = Some(Box::new(commit_details.clone())),
                Some(cd) => cd.merge(commit_details),
            }
        }
    }

    /// `MergeCopExecDetails` merges a CopExecDetails into self.
    // go: SyncExecDetails.MergeCopExecDetails L1
    pub fn merge_cop_exec_details(&self, details: Option<&CopExecDetails>, cop_time: Duration) {
        let Some(details) = details else { return };
        let mut s = self.lock();
        s.exec_details.cop_time = s.exec_details.cop_time.wrapping_add(cop_time);
        s.exec_details.backoff_time = s
            .exec_details
            .backoff_time
            .wrapping_add(details.backoff_time);
        s.exec_details.request_count = s.exec_details.request_count.wrapping_add(1);
        s.merge_scan_detail(Some(&details.scan_detail));
        s.merge_time_detail(details.time_detail);
        let detail = DetailsNeedP90 {
            backoff_sleep: details.backoff_sleep.clone(),
            backoff_times: details.backoff_times.clone(),
            callee_address: details.callee_address.clone(),
            time_detail: details.time_detail,
        };
        s.details_summary.merge(&detail);
    }

    /// `MergeLockKeysExecDetails` merges lock keys execution details into self. The first
    /// detail is stored as a copy (Go stores the pointer, block spec §4).
    // go: SyncExecDetails.MergeLockKeysExecDetails L2 (lockKeys is stored by value)
    pub fn merge_lock_keys_exec_details(&self, lock_keys: Option<&LockKeysDetails>) {
        let mut s = self.lock();
        merge_lock_keys(&mut s.exec_details.lock_keys_detail, lock_keys);
    }

    /// `MergeSharedLockKeysExecDetails` merges shared lock keys execution details into self.
    // go: SyncExecDetails.MergeSharedLockKeysExecDetails L2 (lockKeys is stored by value)
    pub fn merge_shared_lock_keys_exec_details(&self, lock_keys: Option<&LockKeysDetails>) {
        let mut s = self.lock();
        merge_lock_keys(&mut s.exec_details.shared_lock_keys_detail, lock_keys);
    }

    /// `Reset` resets the content inside.
    // go: SyncExecDetails.Reset L1
    pub fn reset(&self) {
        let mut s = self.lock();
        s.exec_details = ExecDetails::default();
        s.details_summary.reset();
    }

    /// `GetExecDetails` returns the exec details inside (a copy).
    // go: SyncExecDetails.GetExecDetails L1
    pub fn get_exec_details(&self) -> ExecDetails {
        self.lock().exec_details.clone()
    }

    /// `CopTasksDetails` returns some useful information of cop-tasks during execution.
    // go: SyncExecDetails.CopTasksDetails L1
    pub fn cop_tasks_details(&self) -> Option<CopTasksDetails> {
        let mut guard = self.lock();
        let s = &mut *guard;
        let n = s.details_summary.num_cop_tasks;
        if n == 0 {
            return None;
        }
        let mut d = CopTasksDetails {
            num_cop_tasks: n,
            ..Default::default()
        };
        d.avg_process_time = s
            .exec_details
            .details_need_p90
            .time_detail
            .process_time
            .wrapping_div(n);
        d.avg_wait_time = s
            .exec_details
            .details_need_p90
            .time_detail
            .wait_time
            .wrapping_div(n);

        d.p90_process_time = f64_to_i64(
            s.details_summary
                .process_time_percentile
                .get_percentile(0.9),
        ); // [M-F2I]
        d.max_process_time = s.details_summary.process_time_percentile.get_max().d;
        d.max_process_address = s
            .details_summary
            .process_time_percentile
            .get_max()
            .addr
            .clone();

        d.p90_wait_time = f64_to_i64(s.details_summary.wait_time_percentile.get_percentile(0.9)); // [M-F2I]
        d.max_wait_time = s.details_summary.wait_time_percentile.get_max().d;
        d.max_wait_address = s
            .details_summary
            .wait_time_percentile
            .get_max()
            .addr
            .clone();

        let mut empty = BTreeMap::new();
        let backoff_info = s
            .details_summary
            .backoff_info
            .as_mut()
            .unwrap_or(&mut empty);
        if !backoff_info.is_empty() {
            d.max_backoff_time = Some(BTreeMap::new());
            d.avg_backoff_time = Some(BTreeMap::new());
            d.p90_backoff_time = Some(BTreeMap::new());
            d.tot_backoff_time = Some(BTreeMap::new());
            d.tot_backoff_times = Some(BTreeMap::new());
            d.max_backoff_address = Some(BTreeMap::new());
        }
        for (backoff, items) in backoff_info.iter_mut() {
            let n = items.req_times;
            let max = items.backoff_percentile.get_max().clone();
            d.max_backoff_address
                .as_mut()
                .unwrap()
                .insert(backoff.clone(), max.addr);
            d.max_backoff_time
                .as_mut()
                .unwrap()
                .insert(backoff.clone(), max.d);
            let p90 = f64_to_i64(items.backoff_percentile.get_percentile(0.9)); // [M-F2I]
            d.p90_backoff_time
                .as_mut()
                .unwrap()
                .insert(backoff.clone(), p90);

            d.avg_backoff_time
                .as_mut()
                .unwrap()
                .insert(backoff.clone(), items.tot_backoff_time.wrapping_div(n));
            d.tot_backoff_time
                .as_mut()
                .unwrap()
                .insert(backoff.clone(), items.tot_backoff_time);
            d.tot_backoff_times
                .as_mut()
                .unwrap()
                .insert(backoff.clone(), items.tot_backoff_times);
        }
        Some(d)
    }

    /// `CopTasksSummary` returns some summary information of cop-tasks for statement summary.
    // go: SyncExecDetails.CopTasksSummary L1
    pub fn cop_tasks_summary(&self) -> Option<CopTasksSummary> {
        let s = self.lock();
        let n = s.details_summary.num_cop_tasks;
        if n == 0 {
            return None;
        }
        Some(CopTasksSummary {
            num_cop_tasks: n,
            max_process_address: s
                .details_summary
                .process_time_percentile
                .get_max()
                .addr
                .clone(),
            max_process_time: s.details_summary.process_time_percentile.get_max().d,
            tot_process_time: s.exec_details.details_need_p90.time_detail.process_time,
            max_wait_address: s
                .details_summary
                .wait_time_percentile
                .get_max()
                .addr
                .clone(),
            max_wait_time: s.details_summary.wait_time_percentile.get_max().d,
            tot_wait_time: s.exec_details.details_need_p90.time_detail.wait_time,
        })
    }
}

/// The body of `MergeLockKeysExecDetails` / `MergeSharedLockKeysExecDetails`: a nil `lockKeys`
/// is stored while the field is nil, and merging it into a stored detail panics (nil pointer
/// dereference), as in Go.
// glue: shared body of the two lock-keys merges
fn merge_lock_keys(dst: &mut Option<Box<LockKeysDetails>>, lock_keys: Option<&LockKeysDetails>) {
    match dst {
        None => *dst = lock_keys.map(|l| Box::new(l.clone())),
        Some(d) => d.merge(
            lock_keys.expect("runtime error: invalid memory address or nil pointer dereference"),
        ),
    }
}

impl SyncExecDetailsInner {
    /// `mergeScanDetail` merges scan details into self.
    // go: SyncExecDetails.mergeScanDetail L1
    fn merge_scan_detail(&mut self, scan_detail: Option<&ScanDetail>) {
        // Currently TiFlash cop task does not fill scanDetail, so need to skip it if scanDetail is nil
        let Some(scan_detail) = scan_detail else {
            return;
        };
        self.exec_details
            .scan_detail
            .get_or_insert_with(Default::default)
            .merge(scan_detail);
    }

    /// `mergeTimeDetail` merges time details into self.
    // go: SyncExecDetails.mergeTimeDetail L1
    fn merge_time_detail(&mut self, time_detail: TimeDetail) {
        let td = &mut self.exec_details.details_need_p90.time_detail;
        td.process_time = td.process_time.wrapping_add(time_detail.process_time);
        td.wait_time = td.wait_time.wrapping_add(time_detail.wait_time);
    }
}

/// `CopTasksDetails` collects some useful information of cop-tasks during execution. Go's
/// nil maps (no backoff) are `None`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CopTasksDetails {
    pub num_cop_tasks: i64,

    pub avg_process_time: Duration,
    pub p90_process_time: Duration,
    pub max_process_address: String,
    pub max_process_time: Duration,

    pub avg_wait_time: Duration,
    pub p90_wait_time: Duration,
    pub max_wait_address: String,
    pub max_wait_time: Duration,

    pub max_backoff_time: Option<BTreeMap<String, Duration>>,
    pub max_backoff_address: Option<BTreeMap<String, String>>,
    pub avg_backoff_time: Option<BTreeMap<String, Duration>>,
    pub p90_backoff_time: Option<BTreeMap<String, Duration>>,
    pub tot_backoff_time: Option<BTreeMap<String, Duration>>,
    pub tot_backoff_times: Option<BTreeMap<String, i64>>,
}

/// `CopTasksSummary` collects some summary information of cop-tasks for statement summary.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CopTasksSummary {
    pub num_cop_tasks: i64,
    pub max_process_address: String,
    pub max_process_time: Duration,
    pub tot_process_time: Duration,
    pub max_wait_address: String,
    pub max_wait_time: Duration,
    pub tot_wait_time: Duration,
}

impl CopTasksDetails {
    /// `ToZapFields` wraps the CopTasksDetails as zap.Fields (a nil receiver gives none).
    // go: CopTasksDetails.ToZapFields L1
    pub fn to_zap_fields(d: Option<&CopTasksDetails>) -> Vec<LogField> {
        let Some(d) = d else { return Vec::new() };
        if d.num_cop_tasks == 0 {
            return Vec::new();
        }
        vec![
            zap_int("num_cop_tasks", d.num_cop_tasks),
            zap_string("process_avg_time", secs(d.avg_process_time, -1) + "s"),
            zap_string("process_p90_time", secs(d.p90_process_time, -1) + "s"),
            zap_string("process_max_time", secs(d.max_process_time, -1) + "s"),
            zap_string("process_max_addr", d.max_process_address.clone()),
            zap_string("wait_avg_time", secs(d.avg_wait_time, -1) + "s"),
            zap_string("wait_p90_time", secs(d.p90_wait_time, -1) + "s"),
            zap_string("wait_max_time", secs(d.max_wait_time, -1) + "s"),
            zap_string("wait_max_addr", d.max_wait_address.clone()),
        ]
    }
}

/// `basicCopRuntimeStats`.
#[derive(Clone, Debug, Default)]
pub struct BasicCopRuntimeStats {
    loop_: i32,
    rows: i64,
    threads: i32,
    proc_times: Percentile<DurationWrapper>,
    // executor extra infos
    tiflash_stats: Option<Box<TiflashStats>>,
}

/// `TiflashStats` contains tiflash execution stats.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TiflashStats {
    scan_context: TiFlashScanContext,
    wait_summary: TiFlashWaitSummary,
}

/// `canGetFloat64`.
pub trait CanGetFloat64: Clone + Default + std::fmt::Debug {
    // glue: the interface's method declaration (implementations carry the go: annotations)
    fn get_float64(&self) -> f64;
}

/// `Int64` is a wrapper of int64 to implement the canGetFloat64 interface.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Int64(pub i64);

impl CanGetFloat64 for Int64 {
    // go: Int64.GetFloat64 L1
    fn get_float64(&self) -> f64 {
        self.0 as f64
    }
}

/// `Duration` is a wrapper of time.Duration to implement the canGetFloat64 interface.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DurationWrapper(pub Duration);

impl CanGetFloat64 for DurationWrapper {
    // go: Duration.GetFloat64 L1
    fn get_float64(&self) -> f64 {
        self.0 as f64
    }
}

/// `DurationWithAddr` is a wrapper of time.Duration and string to implement the
/// canGetFloat64 interface.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DurationWithAddr {
    pub d: Duration,
    pub addr: String,
}

impl CanGetFloat64 for DurationWithAddr {
    // go: DurationWithAddr.GetFloat64 L1
    fn get_float64(&self) -> f64 {
        self.d as f64
    }
}

/// `Percentile` is a struct to calculate the percentile of a series of values. `Clone` is a
/// deep copy (Go's struct copy shares `values`' backing array and the t-digest; block spec
/// §4).
#[derive(Clone, Debug, Default)]
pub struct Percentile<T: CanGetFloat64> {
    values: Vec<T>,
    size: i64,
    is_sorted: bool,

    min_val: T,
    max_val: T,
    sum_val: f64,
    dt: Option<Box<TDigest>>,
}

impl<T: CanGetFloat64> Percentile<T> {
    /// `Add` adds a value to calculate the percentile.
    // go: Percentile.Add L1
    pub fn add(&mut self, value: T) {
        self.is_sorted = false;
        self.sum_val += value.get_float64();
        self.size = self.size.wrapping_add(1);
        if self.dt.is_none() && self.values.is_empty() {
            self.min_val = value.clone();
            self.max_val = value.clone();
        } else {
            if value.get_float64() < self.min_val.get_float64() {
                self.min_val = value.clone();
            }
            if value.get_float64() > self.max_val.get_float64() {
                self.max_val = value.clone();
            }
        }
        match &mut self.dt {
            None => {
                self.values.push(value);
                if self.values.len() >= MaxDetailsNumsForOneQuery {
                    let mut dt = Box::new(TDigest::new());
                    for v in &self.values {
                        dt.add(v.get_float64(), 1.0);
                    }
                    self.values = Vec::new();
                    self.dt = Some(dt);
                }
            }
            Some(dt) => dt.add(value.get_float64(), 1.0),
        }
    }

    /// `GetPercentile` returns the percentile `f` of the values. Like Go it panics (index out
    /// of range) for `f == 1` or on an empty percentile without a t-digest.
    // go: Percentile.GetPercentile L1
    pub fn get_percentile(&mut self, f: f64) -> f64 {
        match &mut self.dt {
            None => {
                if !self.is_sorted {
                    self.is_sorted = true;
                    sort::sort(&mut self.values, |a, b| a.get_float64() < b.get_float64());
                }
                let idx = f64_to_i64(self.values.len() as f64 * f); // [M-F2I]
                self.values[usize::try_from(idx).expect("index out of range")].get_float64()
            }
            Some(dt) => dt.quantile(f),
        }
    }

    /// `GetMax` returns the max value.
    // go: Percentile.GetMax L1
    pub fn get_max(&self) -> &T {
        &self.max_val
    }

    /// `GetMin` returns the min value.
    // go: Percentile.GetMin L1
    pub fn get_min(&self) -> &T {
        &self.min_val
    }

    /// `MergePercentile` merges two Percentile (it processes `p2`'s t-digest, as Go's).
    // go: Percentile.MergePercentile L1
    pub fn merge_percentile(&mut self, p2: &mut Percentile<T>) {
        self.is_sorted = false;
        let Some(p2dt) = &mut p2.dt else {
            for v in p2.values.clone() {
                self.add(v);
            }
            return;
        };
        self.sum_val += p2.sum_val;
        self.size = self.size.wrapping_add(p2.size);
        if self.dt.is_none() {
            let mut dt = Box::new(TDigest::new());
            for v in &self.values {
                dt.add(v.get_float64(), 1.0);
            }
            self.values = Vec::new();
            self.dt = Some(dt);
        }
        let centroids = p2dt.centroids();
        self.dt
            .as_mut()
            .expect("set above")
            .add_centroid_list(&centroids);
    }

    /// `Size` returns the size of the values.
    // go: Percentile.Size L1
    pub fn size(&self) -> i64 {
        self.size
    }

    /// `Sum` returns the sum of the values.
    // go: Percentile.Sum L1
    pub fn sum(&self) -> f64 {
        self.sum_val
    }
}

impl BasicCopRuntimeStats {
    /// `String` implements the RuntimeStats interface.
    // go: basicCopRuntimeStats.String L1
    fn string_impl(&self) -> String {
        let mut buf = String::with_capacity(16);
        buf.push_str("time:");
        buf.push_str(&format_duration(f64_to_i64(self.proc_times.sum_val))); // [M-F2I]
        buf.push_str(", loops:");
        buf.push_str(&self.loop_.to_string());
        if let Some(ts) = &self.tiflash_stats {
            buf.push_str(", threads:");
            buf.push_str(&self.threads.to_string());
            buf.push_str(", ");
            if ts.wait_summary.can_be_ignored() {
                buf.push_str(&ts.scan_context.string());
            } else {
                buf.push_str(&ts.wait_summary.string());
                buf.push_str(", ");
                buf.push_str(&ts.scan_context.string());
            }
        }
        buf
    }

    /// `Clone` implements the RuntimeStats interface.
    // go: basicCopRuntimeStats.Clone L2 (procTimes is a deep copy)
    fn clone_impl(&self) -> BasicCopRuntimeStats {
        let mut stats = BasicCopRuntimeStats {
            loop_: self.loop_,
            rows: self.rows,
            threads: self.threads,
            proc_times: self.proc_times.clone(),
            tiflash_stats: None,
        };
        if let Some(ts) = &self.tiflash_stats {
            stats.tiflash_stats = Some(Box::new(TiflashStats {
                scan_context: ts.scan_context.clone_ctx(),
                wait_summary: ts.wait_summary.clone_summary(),
            }));
        }
        stats
    }

    /// `Merge` implements the RuntimeStats interface.
    // go: basicCopRuntimeStats.Merge L1
    fn merge_impl(&mut self, rs: &dyn RuntimeStats) {
        let Some(tmp) = rs.as_any().downcast_ref::<BasicCopRuntimeStats>() else {
            return;
        };
        let mut tmp = tmp.clone();
        self.loop_ = self.loop_.wrapping_add(tmp.loop_);
        self.rows = self.rows.wrapping_add(tmp.rows);
        self.threads = self.threads.wrapping_add(tmp.threads);
        if tmp.proc_times.size() > 0 {
            self.proc_times.merge_percentile(&mut tmp.proc_times);
        }
        if let Some(tts) = &tmp.tiflash_stats {
            let ts = self.tiflash_stats.get_or_insert_with(Default::default);
            ts.scan_context.merge(&tts.scan_context);
            ts.wait_summary.merge(&tts.wait_summary);
        }
    }

    /// `mergeExecSummary` likes Merge, but it merges ExecutorExecutionSummary directly. Like
    /// Go it panics (nil pointer dereference) when `num_iterations`, `num_produced_rows` or
    /// `time_processed_ns` is unset.
    // go: basicCopRuntimeStats.mergeExecSummary L1
    pub fn merge_exec_summary(&mut self, summary: &ExecutorExecutionSummary) {
        assert!(
            summary.has_num_iterations()
                && summary.has_num_produced_rows()
                && summary.has_time_processed_ns(),
            "runtime error: invalid memory address or nil pointer dereference"
        );
        self.loop_ = self.loop_.wrapping_add(summary.get_num_iterations() as i32);
        self.rows = self
            .rows
            .wrapping_add(summary.get_num_produced_rows() as i64);
        self.threads = self.threads.wrapping_add(summary.get_concurrency() as i32);
        self.proc_times
            .add(DurationWrapper(summary.get_time_processed_ns() as i64));
        if summary.has_tiflash_scan_context() {
            let c = summary.get_tiflash_scan_context();
            let mut regions_of_instance = BTreeMap::new();
            for instance in c.get_regions_of_instance() {
                regions_of_instance.insert(
                    instance.get_instance_id().to_string(),
                    instance.get_region_num(),
                );
            }
            let tmp = TiFlashScanContext {
                dmfile_data_scanned_rows: c.get_dmfile_data_scanned_rows(),
                dmfile_data_skipped_rows: c.get_dmfile_data_skipped_rows(),
                dmfile_mvcc_scanned_rows: c.get_dmfile_mvcc_scanned_rows(),
                dmfile_mvcc_skipped_rows: c.get_dmfile_mvcc_skipped_rows(),
                dmfile_lm_filter_scanned_rows: c.get_dmfile_lm_filter_scanned_rows(),
                dmfile_lm_filter_skipped_rows: c.get_dmfile_lm_filter_skipped_rows(),
                total_dmfile_rs_check_ms: c.get_total_dmfile_rs_check_ms(),
                total_dmfile_read_ms: c.get_total_dmfile_read_ms(),
                total_build_snapshot_ms: c.get_total_build_snapshot_ms(),
                local_regions: c.get_local_regions(),
                remote_regions: c.get_remote_regions(),
                total_learner_read_ms: c.get_total_learner_read_ms(),
                disagg_read_cache_hit_bytes: c.get_disagg_read_cache_hit_bytes(),
                disagg_read_cache_miss_bytes: c.get_disagg_read_cache_miss_bytes(),
                segments: c.get_segments(),
                read_tasks: c.get_read_tasks(),
                delta_rows: c.get_delta_rows(),
                delta_bytes: c.get_delta_bytes(),
                mvcc_input_rows: c.get_mvcc_input_rows(),
                mvcc_input_bytes: c.get_mvcc_input_bytes(),
                mvcc_output_rows: c.get_mvcc_output_rows(),
                lm_skip_rows: c.get_lm_skip_rows(),
                total_build_bitmap_ms: c.get_total_build_bitmap_ms(),
                total_build_input_stream_ms: c.get_total_build_inputstream_ms(),
                stale_read_regions: c.get_stale_read_regions(),
                min_local_stream_ms: c.get_min_local_stream_ms(),
                max_local_stream_ms: c.get_max_local_stream_ms(),
                min_remote_stream_ms: c.get_min_remote_stream_ms(),
                max_remote_stream_ms: c.get_max_remote_stream_ms(),
                regions_of_instance,

                total_vector_idx_load_from_s3: c.get_vector_idx_load_from_s3(),
                total_vector_idx_load_from_disk: c.get_vector_idx_load_from_disk(),
                total_vector_idx_load_from_cache: c.get_vector_idx_load_from_cache(),
                total_vector_idx_load_time_ms: c.get_vector_idx_load_time_ms(),
                total_vector_idx_search_time_ms: c.get_vector_idx_search_time_ms(),
                total_vector_idx_search_visited_nodes: c.get_vector_idx_search_visited_nodes(),
                total_vector_idx_search_discarded_nodes: c.get_vector_idx_search_discarded_nodes(),
                total_vector_idx_read_vec_time_ms: c.get_vector_idx_read_vec_time_ms(),
                total_vector_idx_read_others_time_ms: c.get_vector_idx_read_others_time_ms(),
            };
            match &mut self.tiflash_stats {
                None => {
                    self.tiflash_stats = Some(Box::new(TiflashStats {
                        scan_context: tmp,
                        ..Default::default()
                    }))
                }
                Some(ts) => ts.scan_context.merge(&tmp),
            }
        }
        if summary.has_tiflash_wait_summary() {
            let w = summary.get_tiflash_wait_summary();
            let tmp = TiFlashWaitSummary {
                execution_time: summary.get_time_processed_ns(),
                min_tso_wait_time: w.get_min_tso_wait_ns(),
                pipeline_breaker_wait_time: w.get_pipeline_breaker_wait_ns(),
                pipeline_queue_wait_time: w.get_pipeline_queue_wait_ns(),
            };
            match &mut self.tiflash_stats {
                None => {
                    self.tiflash_stats = Some(Box::new(TiflashStats {
                        wait_summary: tmp,
                        ..Default::default()
                    }))
                }
                Some(ts) => ts.wait_summary.merge(&tmp),
            }
        }
    }
}

impl RuntimeStats for BasicCopRuntimeStats {
    // glue: RuntimeStats.String -> basicCopRuntimeStats.String
    fn string(&self) -> String {
        self.string_impl()
    }
    // glue: RuntimeStats.Merge -> basicCopRuntimeStats.Merge
    fn merge(&mut self, other: &dyn RuntimeStats) {
        self.merge_impl(other)
    }
    // glue: RuntimeStats.Clone -> basicCopRuntimeStats.Clone
    fn clone_stats(&self) -> Box<dyn RuntimeStats> {
        Box::new(self.clone_impl())
    }
    /// `Tp` implements the RuntimeStats interface.
    // go: basicCopRuntimeStats.Tp L1
    fn tp(&self) -> i64 {
        TpBasicCopRunTimeStats
    }
    // glue: type assertion support
    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// `CopRuntimeStats` collects cop tasks' execution info.
#[derive(Clone, Debug, Default)]
pub struct CopRuntimeStats {
    /// stats stores the runtime statistics of coprocessor tasks.
    stats: BasicCopRuntimeStats,
    scan_detail: ScanDetail,
    time_detail: TimeDetail,
    store_type: StoreType,
}

impl CopRuntimeStats {
    /// `GetActRows` return total rows of CopRuntimeStats.
    // go: CopRuntimeStats.GetActRows L1
    pub fn get_act_rows(&self) -> i64 {
        self.stats.rows
    }

    /// The `basicCopRuntimeStats` the cop stats accumulate (unexported in Go; replay access).
    // glue: accessor of the unexported stats field
    pub fn basic_cop_runtime_stats(&self) -> &BasicCopRuntimeStats {
        &self.stats
    }

    /// `GetTasks` return total tasks of CopRuntimeStats.
    // go: CopRuntimeStats.GetTasks L1
    pub fn get_tasks(&self) -> i32 {
        self.stats.proc_times.size as i32
    }

    /// `String`. It sorts / processes the percentile in place (Go does it through the copy's
    /// shared slice and t-digest pointer), hence `&mut self`.
    // go: CopRuntimeStats.String L1
    pub fn string(&mut self) -> String {
        let total_tasks = self.stats.proc_times.size;
        let is_tiflash_cop = self.store_type == kv::TiFlash;
        let mut buf = String::with_capacity(16);
        {
            let threads = self.stats.threads;
            let tiflash_stats = self.stats.tiflash_stats.clone();
            let print_tiflash_specific_info = |buf: &mut String| {
                if is_tiflash_cop {
                    buf.push_str(", ");
                    buf.push_str("threads:");
                    buf.push_str(&threads.to_string());
                    buf.push('}');
                    if let Some(ts) = &tiflash_stats {
                        if !ts.wait_summary.can_be_ignored() {
                            buf.push_str(", ");
                            buf.push_str(&ts.wait_summary.string());
                        }
                        if !ts.scan_context.empty() {
                            buf.push_str(", ");
                            buf.push_str(&ts.scan_context.string());
                        }
                    }
                } else {
                    buf.push('}');
                }
            };
            let proc_times = &mut self.stats.proc_times;
            if total_tasks == 1 {
                buf.push_str(self.store_type.name());
                buf.push_str("_task:{time:");
                buf.push_str(&format_duration(f64_to_i64(proc_times.get_percentile(0.0)))); // [M-F2I]
                buf.push_str(", loops:");
                buf.push_str(&self.stats.loop_.to_string());
                print_tiflash_specific_info(&mut buf);
            } else if total_tasks > 0 {
                buf.push_str(self.store_type.name());
                buf.push_str("_task:{proc max:");
                buf.push_str(&format_duration(f64_to_i64(
                    proc_times.get_max().get_float64(),
                ))); // [M-F2I]
                buf.push_str(", min:");
                buf.push_str(&format_duration(f64_to_i64(
                    proc_times.get_min().get_float64(),
                ))); // [M-F2I]
                buf.push_str(", avg: ");
                buf.push_str(&format_duration(
                    f64_to_i64(proc_times.sum()).wrapping_div(total_tasks),
                )); // [M-F2I]
                buf.push_str(", p80:");
                buf.push_str(&format_duration(f64_to_i64(proc_times.get_percentile(0.8)))); // [M-F2I]
                buf.push_str(", p95:");
                buf.push_str(&format_duration(f64_to_i64(
                    proc_times.get_percentile(0.95),
                ))); // [M-F2I]
                buf.push_str(", iters:");
                buf.push_str(&self.stats.loop_.to_string());
                buf.push_str(", tasks:");
                buf.push_str(&total_tasks.to_string());
                print_tiflash_specific_info(&mut buf);
            }
        }
        if !is_tiflash_cop {
            let detail = self.scan_detail.string();
            if !detail.is_empty() {
                buf.push_str(", ");
                buf.push_str(&detail);
            }
            if self.time_detail != TimeDetail::default() {
                let time_detail_str = self.time_detail.string();
                if !time_detail_str.is_empty() {
                    buf.push_str(", ");
                    buf.push_str(&time_detail_str);
                }
            }
        }
        buf
    }
}

/// TpBasicRuntimeStats is the tp for BasicRuntimeStats.
pub const TpBasicRuntimeStats: i64 = 0;
/// TpRuntimeStatsWithCommit is the tp for RuntimeStatsWithCommit.
pub const TpRuntimeStatsWithCommit: i64 = 1;
/// TpRuntimeStatsWithConcurrencyInfo is the tp for RuntimeStatsWithConcurrencyInfo.
pub const TpRuntimeStatsWithConcurrencyInfo: i64 = 2;
/// TpSnapshotRuntimeStats is the tp for SnapshotRuntimeStats.
pub const TpSnapshotRuntimeStats: i64 = 3;
/// TpHashJoinRuntimeStats is the tp for HashJoinRuntimeStats.
pub const TpHashJoinRuntimeStats: i64 = 4;
/// TpIndexLookUpJoinRuntimeStats is the tp for IndexLookUpJoinRuntimeStats.
pub const TpIndexLookUpJoinRuntimeStats: i64 = 5;
/// TpRuntimeStatsWithSnapshot is the tp for RuntimeStatsWithSnapshot.
pub const TpRuntimeStatsWithSnapshot: i64 = 6;
/// TpJoinRuntimeStats is the tp for JoinRuntimeStats.
pub const TpJoinRuntimeStats: i64 = 7;
/// TpSelectResultRuntimeStats is the tp for SelectResultRuntimeStats.
pub const TpSelectResultRuntimeStats: i64 = 8;
/// TpInsertRuntimeStat is the tp for InsertRuntimeStat
pub const TpInsertRuntimeStat: i64 = 9;
/// TpIndexLookUpRunTimeStats is the tp for IndexLookUpRunTimeStats
pub const TpIndexLookUpRunTimeStats: i64 = 10;
/// TpSlowQueryRuntimeStat is the tp for SlowQueryRuntimeStat
pub const TpSlowQueryRuntimeStat: i64 = 11;
/// TpHashAggRuntimeStat is the tp for HashAggRuntimeStat
pub const TpHashAggRuntimeStat: i64 = 12;
/// TpIndexMergeRunTimeStats is the tp for IndexMergeRunTimeStats
pub const TpIndexMergeRunTimeStats: i64 = 13;
/// TpBasicCopRunTimeStats is the tp for BasicCopRunTimeStats
pub const TpBasicCopRunTimeStats: i64 = 14;
/// TpUpdateRuntimeStats is the tp for UpdateRuntimeStats
pub const TpUpdateRuntimeStats: i64 = 15;
/// TpFKCheckRuntimeStats is the tp for FKCheckRuntimeStats
pub const TpFKCheckRuntimeStats: i64 = 16;
/// TpFKCascadeRuntimeStats is the tp for FKCascadeRuntimeStats
pub const TpFKCascadeRuntimeStats: i64 = 17;
/// TpRURuntimeStats is the tp for RURuntimeStats
pub const TpRURuntimeStats: i64 = 18;

/// `RuntimeStats` is used to express the executor runtime information.
pub trait RuntimeStats: Any + Send + Sync + std::fmt::Debug {
    // glue: interface method String
    fn string(&self) -> String;
    // glue: interface method Merge
    fn merge(&mut self, other: &dyn RuntimeStats);
    /// Go's `Clone() RuntimeStats`.
    // glue: interface method Clone
    fn clone_stats(&self) -> Box<dyn RuntimeStats>;
    // glue: interface method Tp
    fn tp(&self) -> i64;
    /// Go's type assertions on the interface (`rs.(*T)`).
    // glue: type assertion support
    fn as_any(&self) -> &dyn Any;
}

/// `TiFlashScanContext` is used to express the table scan information in tiflash.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TiFlashScanContext {
    pub dmfile_data_scanned_rows: u64,
    pub dmfile_data_skipped_rows: u64,
    pub dmfile_mvcc_scanned_rows: u64,
    pub dmfile_mvcc_skipped_rows: u64,
    pub dmfile_lm_filter_scanned_rows: u64,
    pub dmfile_lm_filter_skipped_rows: u64,
    pub total_dmfile_rs_check_ms: u64,
    pub total_dmfile_read_ms: u64,
    pub total_build_snapshot_ms: u64,
    pub local_regions: u64,
    pub remote_regions: u64,
    pub total_learner_read_ms: u64,
    pub disagg_read_cache_hit_bytes: u64,
    pub disagg_read_cache_miss_bytes: u64,
    pub segments: u64,
    pub read_tasks: u64,
    pub delta_rows: u64,
    pub delta_bytes: u64,
    pub mvcc_input_rows: u64,
    pub mvcc_input_bytes: u64,
    pub mvcc_output_rows: u64,
    pub lm_skip_rows: u64,
    pub total_build_bitmap_ms: u64,
    pub total_build_input_stream_ms: u64,
    pub stale_read_regions: u64,
    pub min_local_stream_ms: u64,
    pub max_local_stream_ms: u64,
    pub min_remote_stream_ms: u64,
    pub max_remote_stream_ms: u64,
    pub regions_of_instance: BTreeMap<String, u64>,

    pub total_vector_idx_load_from_s3: u64,
    pub total_vector_idx_load_from_disk: u64,
    pub total_vector_idx_load_from_cache: u64,
    pub total_vector_idx_load_time_ms: u64,
    pub total_vector_idx_search_time_ms: u64,
    pub total_vector_idx_search_visited_nodes: u64,
    pub total_vector_idx_search_discarded_nodes: u64,
    pub total_vector_idx_read_vec_time_ms: u64,
    pub total_vector_idx_read_others_time_ms: u64,
}

impl TiFlashScanContext {
    /// `Clone` implements the deep copy of `*TiFlashScanContext`.
    // go: TiFlashScanContext.Clone L1
    pub fn clone_ctx(&self) -> TiFlashScanContext {
        self.clone()
    }

    // go: TiFlashScanContext.String L1
    pub fn string(&self) -> String {
        let c = self;
        let mut output: Vec<String> = Vec::new();
        if c.total_vector_idx_load_from_s3
            .wrapping_add(c.total_vector_idx_load_from_disk)
            .wrapping_add(c.total_vector_idx_load_from_cache)
            > 0
        {
            let items = [
                format!(
                    "load:{{total:{}ms,from_s3:{},from_disk:{},from_cache:{}}}",
                    c.total_vector_idx_load_time_ms,
                    c.total_vector_idx_load_from_s3,
                    c.total_vector_idx_load_from_disk,
                    c.total_vector_idx_load_from_cache
                ),
                format!(
                    "search:{{total:{}ms,visited_nodes:{},discarded_nodes:{}}}",
                    c.total_vector_idx_search_time_ms,
                    c.total_vector_idx_search_visited_nodes,
                    c.total_vector_idx_search_discarded_nodes
                ),
                format!(
                    "read:{{vec_total:{}ms,others_total:{}ms}}",
                    c.total_vector_idx_read_vec_time_ms, c.total_vector_idx_read_others_time_ms
                ),
            ];
            output.push(format!("vector_idx:{{{}}}", items.join(",")));
        }

        let mut region_balance_info = "none".to_string();
        if !c.regions_of_instance.is_empty() {
            let mut max_num = 0u64;
            let mut min_num = u64::MAX;
            for &v in c.regions_of_instance.values() {
                if v > max_num {
                    max_num = v;
                }
                if v > 0 && v < min_num {
                    min_num = v;
                }
            }
            region_balance_info = format!(
                "{{instance_num: {}, max/min: {}/{}={}}}",
                c.regions_of_instance.len(),
                max_num,
                min_num,
                format_float(max_num as f64 / min_num as f64, b'f', 6, 64)
            );
        }
        let mut dmfile_disagg_info = String::new();
        if c.disagg_read_cache_hit_bytes != 0 || c.disagg_read_cache_miss_bytes != 0 {
            dmfile_disagg_info = format!(
                ", disagg_cache_hit_bytes: {}, disagg_cache_miss_bytes: {}",
                c.disagg_read_cache_hit_bytes, c.disagg_read_cache_miss_bytes
            );
        }
        let mut remote_stream_info = String::new();
        if c.min_remote_stream_ms != 0 || c.max_remote_stream_ms != 0 {
            remote_stream_info = format!(
                "min_remote_stream:{}ms, max_remote_stream:{}ms, ",
                c.min_remote_stream_ms, c.max_remote_stream_ms
            );
        }

        // note: "tot" is short for "total"
        output.push(format!(
            "tiflash_scan:{{mvcc_input_rows:{}, mvcc_input_bytes:{}, mvcc_output_rows:{}, lm_skip_rows:{}, \
             local_regions:{}, remote_regions:{}, tot_learner_read:{}ms, region_balance:{}, delta_rows:{}, \
             delta_bytes:{}, segments:{}, stale_read_regions:{}, tot_build_snapshot:{}ms, tot_build_bitmap:{}ms, \
             tot_build_inputstream:{}ms, min_local_stream:{}ms, max_local_stream:{}ms, {}dtfile:{{\
             data_scanned_rows:{}, data_skipped_rows:{}, mvcc_scanned_rows:{}, mvcc_skipped_rows:{}, \
             lm_filter_scanned_rows:{}, lm_filter_skipped_rows:{}, tot_rs_index_check:{}ms, tot_read:{}ms{}}}}}",
            c.mvcc_input_rows,
            c.mvcc_input_bytes,
            c.mvcc_output_rows,
            c.lm_skip_rows,
            c.local_regions,
            c.remote_regions,
            c.total_learner_read_ms,
            region_balance_info,
            c.delta_rows,
            c.delta_bytes,
            c.segments,
            c.stale_read_regions,
            c.total_build_snapshot_ms,
            c.total_build_bitmap_ms,
            c.total_build_input_stream_ms,
            c.min_local_stream_ms,
            c.max_local_stream_ms,
            remote_stream_info,
            c.dmfile_data_scanned_rows,
            c.dmfile_data_skipped_rows,
            c.dmfile_mvcc_scanned_rows,
            c.dmfile_mvcc_skipped_rows,
            c.dmfile_lm_filter_scanned_rows,
            c.dmfile_lm_filter_skipped_rows,
            c.total_dmfile_rs_check_ms,
            c.total_dmfile_read_ms,
            dmfile_disagg_info,
        ));

        output.join(", ")
    }

    /// `Merge` make sum to merge the information in TiFlashScanContext.
    // go: TiFlashScanContext.Merge L1
    pub fn merge(&mut self, other: &TiFlashScanContext) {
        macro_rules! add {
            ($($f:ident),*) => { $( self.$f = self.$f.wrapping_add(other.$f); )* };
        }
        add!(
            dmfile_data_scanned_rows,
            dmfile_data_skipped_rows,
            dmfile_mvcc_scanned_rows,
            dmfile_mvcc_skipped_rows,
            dmfile_lm_filter_scanned_rows,
            dmfile_lm_filter_skipped_rows,
            total_dmfile_rs_check_ms,
            total_dmfile_read_ms,
            total_build_snapshot_ms,
            local_regions,
            remote_regions,
            total_learner_read_ms,
            disagg_read_cache_hit_bytes,
            disagg_read_cache_miss_bytes,
            segments,
            read_tasks,
            delta_rows,
            delta_bytes,
            mvcc_input_rows,
            mvcc_input_bytes,
            mvcc_output_rows,
            lm_skip_rows,
            total_build_bitmap_ms,
            total_build_input_stream_ms,
            stale_read_regions,
            total_vector_idx_load_from_s3,
            total_vector_idx_load_from_disk,
            total_vector_idx_load_from_cache,
            total_vector_idx_load_time_ms,
            total_vector_idx_search_time_ms,
            total_vector_idx_search_visited_nodes,
            total_vector_idx_search_discarded_nodes,
            total_vector_idx_read_vec_time_ms,
            total_vector_idx_read_others_time_ms
        );

        if self.min_local_stream_ms == 0 || other.min_local_stream_ms < self.min_local_stream_ms {
            self.min_local_stream_ms = other.min_local_stream_ms;
        }
        if other.max_local_stream_ms > self.max_local_stream_ms {
            self.max_local_stream_ms = other.max_local_stream_ms;
        }
        if self.min_remote_stream_ms == 0 || other.min_remote_stream_ms < self.min_remote_stream_ms
        {
            self.min_remote_stream_ms = other.min_remote_stream_ms;
        }
        if other.max_remote_stream_ms > self.max_remote_stream_ms {
            self.max_remote_stream_ms = other.max_remote_stream_ms;
        }

        for (k, v) in &other.regions_of_instance {
            let e = self.regions_of_instance.entry(k.clone()).or_insert(0);
            *e = e.wrapping_add(*v);
        }
    }

    /// `Empty` check whether TiFlashScanContext is Empty, if scan no pack and skip no pack, we
    /// regard it as empty.
    // go: TiFlashScanContext.Empty L1
    pub fn empty(&self) -> bool {
        self.dmfile_data_scanned_rows == 0
            && self.dmfile_data_skipped_rows == 0
            && self.dmfile_mvcc_scanned_rows == 0
            && self.dmfile_mvcc_skipped_rows == 0
            && self.dmfile_lm_filter_scanned_rows == 0
            && self.dmfile_lm_filter_skipped_rows == 0
            && self.local_regions == 0
            && self.remote_regions == 0
            && self.total_vector_idx_load_from_disk == 0
            && self.total_vector_idx_load_from_cache == 0
            && self.total_vector_idx_load_from_s3 == 0
    }
}

/// `TiFlashWaitSummary` is used to express all kinds of wait information in tiflash.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TiFlashWaitSummary {
    /// keep execution time to do merge work, always record the wait time with largest execution time
    pub execution_time: u64,
    pub min_tso_wait_time: u64,
    pub pipeline_breaker_wait_time: u64,
    pub pipeline_queue_wait_time: u64,
}

impl TiFlashWaitSummary {
    /// `Clone` implements the deep copy of `*TiFlashWaitSummary`.
    // go: TiFlashWaitSummary.Clone L1
    pub fn clone_summary(&self) -> TiFlashWaitSummary {
        *self
    }

    /// `String` dumps TiFlashWaitSummary info as string.
    // go: TiFlashWaitSummary.String L1
    pub fn string(&self) -> String {
        if self.can_be_ignored() {
            return String::new();
        }
        let ms = MILLISECOND as u64;
        let mut buf = String::with_capacity(32);
        buf.push_str("tiflash_wait: {");
        let mut empty = true;
        if self.min_tso_wait_time >= ms {
            buf.push_str("minTSO_wait: ");
            buf.push_str(&duration_milliseconds(self.min_tso_wait_time as i64).to_string());
            buf.push_str("ms");
            empty = false;
        }
        if self.pipeline_breaker_wait_time >= ms {
            if !empty {
                buf.push_str(", ");
            }
            buf.push_str("pipeline_breaker_wait: ");
            buf.push_str(
                &duration_milliseconds(self.pipeline_breaker_wait_time as i64).to_string(),
            );
            buf.push_str("ms");
            empty = false;
        }
        if self.pipeline_queue_wait_time >= ms {
            if !empty {
                buf.push_str(", ");
            }
            buf.push_str("pipeline_queue_wait: ");
            buf.push_str(&duration_milliseconds(self.pipeline_queue_wait_time as i64).to_string());
            buf.push_str("ms");
        }
        buf.push('}');
        buf
    }

    /// `Merge` make sum to merge the information in TiFlashWaitSummary.
    // go: TiFlashWaitSummary.Merge L1
    pub fn merge(&mut self, other: &TiFlashWaitSummary) {
        if self.execution_time < other.execution_time {
            *self = *other;
        }
    }

    /// `CanBeIgnored` check whether TiFlashWaitSummary can be ignored, not all tidb executors
    /// have significant tiflash wait summary.
    // go: TiFlashWaitSummary.CanBeIgnored L1
    pub fn can_be_ignored(&self) -> bool {
        let ms = MILLISECOND as u64;
        self.min_tso_wait_time < ms
            && self.pipeline_breaker_wait_time < ms
            && self.pipeline_queue_wait_time < ms
    }
}

/// `time.Duration.Milliseconds`.
// go-std: time.Duration.Milliseconds L1
fn duration_milliseconds(d: Duration) -> i64 {
    d / 1_000_000
}

/// `BasicRuntimeStats` is the basic runtime stats.
#[derive(Debug, Default)]
pub struct BasicRuntimeStats {
    /// executor's Next() called times.
    loop_: AtomicI32,
    /// executor consume time.
    consume: AtomicI64,
    /// executor return row count.
    rows: AtomicI64,
}

impl BasicRuntimeStats {
    /// `GetActRows` return total rows of BasicRuntimeStats.
    // go: BasicRuntimeStats.GetActRows L1
    pub fn get_act_rows(&self) -> i64 {
        self.rows.load(Ordering::Relaxed)
    }

    /// `Record` records executor's execution.
    // go: BasicRuntimeStats.Record L1
    pub fn record(&self, d: Duration, row_num: i64) {
        self.loop_.fetch_add(1, Ordering::Relaxed);
        self.consume.fetch_add(d, Ordering::Relaxed);
        self.rows.fetch_add(row_num, Ordering::Relaxed);
    }

    /// `SetRowNum` sets the row num.
    // go: BasicRuntimeStats.SetRowNum L1
    pub fn set_row_num(&self, row_num: i64) {
        self.rows.store(row_num, Ordering::Relaxed);
    }

    /// `GetTime` get the int64 total time.
    // go: BasicRuntimeStats.GetTime L1
    pub fn get_time(&self) -> i64 {
        self.consume.load(Ordering::Relaxed)
    }

    // glue: field-wise snapshot copy (Go's Clone body)
    fn snapshot(&self) -> BasicRuntimeStats {
        let result = BasicRuntimeStats::default();
        result
            .loop_
            .store(self.loop_.load(Ordering::Relaxed), Ordering::Relaxed);
        result
            .consume
            .store(self.consume.load(Ordering::Relaxed), Ordering::Relaxed);
        result
            .rows
            .store(self.rows.load(Ordering::Relaxed), Ordering::Relaxed);
        result
    }

    /// `String` implements the RuntimeStats interface (a nil receiver prints "").
    // go: BasicRuntimeStats.String L1
    pub fn string_of(e: Option<&BasicRuntimeStats>) -> String {
        let Some(e) = e else { return String::new() };
        let mut s = String::new();
        s.push_str("time:");
        s.push_str(&format_duration(e.consume.load(Ordering::Relaxed)));
        s.push_str(", loops:");
        s.push_str(&(e.loop_.load(Ordering::Relaxed) as i64).to_string());
        s
    }
}

impl RuntimeStats for BasicRuntimeStats {
    // glue: RuntimeStats.String -> BasicRuntimeStats.String
    fn string(&self) -> String {
        BasicRuntimeStats::string_of(Some(self))
    }
    /// `Merge` implements the RuntimeStats interface.
    // go: BasicRuntimeStats.Merge L1
    fn merge(&mut self, rs: &dyn RuntimeStats) {
        let Some(tmp) = rs.as_any().downcast_ref::<BasicRuntimeStats>() else {
            return;
        };
        self.loop_
            .fetch_add(tmp.loop_.load(Ordering::Relaxed), Ordering::Relaxed);
        self.consume
            .fetch_add(tmp.consume.load(Ordering::Relaxed), Ordering::Relaxed);
        self.rows
            .fetch_add(tmp.rows.load(Ordering::Relaxed), Ordering::Relaxed);
    }
    /// `Clone` implements the RuntimeStats interface.
    // go: BasicRuntimeStats.Clone L1
    fn clone_stats(&self) -> Box<dyn RuntimeStats> {
        Box::new(self.snapshot())
    }
    /// `Tp` implements the RuntimeStats interface.
    // go: BasicRuntimeStats.Tp L1
    fn tp(&self) -> i64 {
        TpBasicRuntimeStats
    }
    // glue: type assertion support
    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// `RootRuntimeStats` is the executor runtime stats that combine with multiple runtime stats.
#[derive(Debug, Default)]
pub struct RootRuntimeStats {
    mu: Mutex<RootRuntimeStatsInner>,
}

/// The fields of [`RootRuntimeStats`].
#[derive(Debug, Default)]
pub struct RootRuntimeStatsInner {
    pub basic: Option<Arc<BasicRuntimeStats>>,
    pub group_rss: Vec<Box<dyn RuntimeStats>>,
}

impl RootRuntimeStats {
    /// `NewRootRuntimeStats` returns a new RootRuntimeStats.
    // go: NewRootRuntimeStats L1
    pub fn new() -> RootRuntimeStats {
        RootRuntimeStats::default()
    }

    // glue: lock the mutex (M-SYNC)
    fn lock(&self) -> std::sync::MutexGuard<'_, RootRuntimeStatsInner> {
        self.mu.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `GetActRows` return total rows of RootRuntimeStats.
    // go: RootRuntimeStats.GetActRows L1
    pub fn get_act_rows(&self) -> i64 {
        match &self.lock().basic {
            None => 0,
            Some(b) => b.rows.load(Ordering::Relaxed),
        }
    }

    /// `MergeStats` merges stats in the RootRuntimeStats and return the stats suitable for
    /// display directly: `f` sees the basic stats and the groups.
    // go: RootRuntimeStats.MergeStats L2 (the slice is lent to a closure)
    pub fn merge_stats<R>(
        &self,
        f: impl FnOnce(Option<&BasicRuntimeStats>, &[Box<dyn RuntimeStats>]) -> R,
    ) -> R {
        let inner = self.lock();
        f(inner.basic.as_deref(), &inner.group_rss)
    }

    /// `String` implements the RuntimeStats interface.
    // go: RootRuntimeStats.String L1
    pub fn string(&self) -> String {
        self.merge_stats(|basic, groups| {
            let mut strs: Vec<String> = Vec::with_capacity(groups.len() + 1);
            if let Some(basic) = basic {
                strs.push(BasicRuntimeStats::string_of(Some(basic)));
            }
            for group in groups {
                let s = group.string();
                if !s.is_empty() {
                    strs.push(s);
                }
            }
            strs.join(", ")
        })
    }
}

/// `RuntimeStatsColl` collects executors's execution info.
#[derive(Debug, Default)]
pub struct RuntimeStatsColl {
    mu: Mutex<RuntimeStatsCollInner>,
}

#[derive(Debug, Default)]
struct RuntimeStatsCollInner {
    root_stats: BTreeMap<i64, Arc<RootRuntimeStats>>,
    cop_stats: BTreeMap<i64, Arc<Mutex<CopRuntimeStats>>>,
}

impl RuntimeStatsColl {
    /// `NewRuntimeStatsColl` creates new executor collector. Reuse the object to reduce
    /// allocation when `reuse` is given.
    // go: NewRuntimeStatsColl L1
    pub fn new(reuse: Option<Arc<RuntimeStatsColl>>) -> Arc<RuntimeStatsColl> {
        if let Some(reuse) = reuse {
            {
                let mut inner = reuse.lock();
                inner.root_stats.clear();
                inner.cop_stats.clear();
            }
            return reuse;
        }
        Arc::new(RuntimeStatsColl::default())
    }

    // glue: lock the mutex (M-SYNC)
    fn lock(&self) -> std::sync::MutexGuard<'_, RuntimeStatsCollInner> {
        self.mu.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `RegisterStats` register execStat for a executor. `info` is moved into the collection
    /// (Go keeps the caller's pointer and merges later registrations into it, block spec §4).
    // go: RuntimeStatsColl.RegisterStats L2 (info is moved in)
    pub fn register_stats(&self, plan_id: i64, info: Box<dyn RuntimeStats>) {
        let mut inner = self.lock();
        let stats = inner
            .root_stats
            .entry(plan_id)
            .or_insert_with(|| Arc::new(RootRuntimeStats::new()))
            .clone();
        let mut root = stats.lock();
        let tp = info.tp();
        for rss in root.group_rss.iter_mut() {
            if rss.tp() == tp {
                rss.merge(info.as_ref());
                return;
            }
        }
        root.group_rss.push(info);
    }

    /// `GetBasicRuntimeStats` gets basicRuntimeStats for a executor.
    // go: RuntimeStatsColl.GetBasicRuntimeStats L1
    pub fn get_basic_runtime_stats(&self, plan_id: i64) -> Arc<BasicRuntimeStats> {
        let mut inner = self.lock();
        let stats = inner
            .root_stats
            .entry(plan_id)
            .or_insert_with(|| Arc::new(RootRuntimeStats::new()))
            .clone();
        let mut root = stats.lock();
        root.basic
            .get_or_insert_with(|| Arc::new(BasicRuntimeStats::default()))
            .clone()
    }

    /// `GetRootStats` gets execStat for a executor.
    // go: RuntimeStatsColl.GetRootStats L1
    pub fn get_root_stats(&self, plan_id: i64) -> Arc<RootRuntimeStats> {
        let mut inner = self.lock();
        inner
            .root_stats
            .entry(plan_id)
            .or_insert_with(|| Arc::new(RootRuntimeStats::new()))
            .clone()
    }

    /// `GetPlanActRows` returns the actual rows of the plan.
    // go: RuntimeStatsColl.GetPlanActRows L1
    pub fn get_plan_act_rows(&self, plan_id: i64) -> i64 {
        let inner = self.lock();
        match inner.root_stats.get(&plan_id) {
            None => 0,
            Some(rs) => rs.get_act_rows(),
        }
    }

    /// `GetCopStats` gets the CopRuntimeStats specified by planID.
    // go: RuntimeStatsColl.GetCopStats L1
    pub fn get_cop_stats(&self, plan_id: i64) -> Option<Arc<Mutex<CopRuntimeStats>>> {
        self.lock().cop_stats.get(&plan_id).cloned()
    }

    /// `GetCopCountAndRows` returns the total cop-tasks count and total rows of all cop-tasks.
    // go: RuntimeStatsColl.GetCopCountAndRows L1
    pub fn get_cop_count_and_rows(&self, plan_id: i64) -> (i32, i64) {
        let inner = self.lock();
        match inner.cop_stats.get(&plan_id) {
            None => (0, 0),
            Some(c) => {
                let c = c.lock().unwrap_or_else(|e| e.into_inner());
                (c.get_tasks(), c.get_act_rows())
            }
        }
    }

    /// `RecordCopStats` records a specific cop tasks's execution detail.
    // go: RuntimeStatsColl.RecordCopStats L1
    pub fn record_cop_stats(
        &self,
        plan_id: i64,
        store_type: StoreType,
        scan: Option<&ScanDetail>,
        time: TimeDetail,
        summary: Option<&ExecutorExecutionSummary>,
    ) -> i64 {
        let mut plan_id = plan_id;
        let mut inner = self.lock();
        let mut cop_stats = match inner.cop_stats.get(&plan_id) {
            None => {
                let mut c = CopRuntimeStats {
                    time_detail: time,
                    store_type,
                    ..Default::default()
                };
                if let Some(scan) = scan {
                    c.scan_detail = *scan;
                }
                let c = Arc::new(Mutex::new(c));
                inner.cop_stats.insert(plan_id, c.clone());
                c
            }
            Some(c) => {
                {
                    let mut g = c.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(scan) = scan {
                        g.scan_detail.merge(scan);
                    }
                    g.time_detail.merge(Some(&time));
                }
                c.clone()
            }
        };
        if let Some(summary) = summary {
            // for TiFlash cop response, ExecutorExecutionSummary contains executor id, so if
            // there is a valid executor id in summary, use it overwrite the planID
            if let Some(id) = get_plan_id_from_execution_summary(summary)
                && id != plan_id
            {
                plan_id = id;
                cop_stats = inner
                    .cop_stats
                    .entry(plan_id)
                    .or_insert_with(|| {
                        Arc::new(Mutex::new(CopRuntimeStats {
                            store_type,
                            ..Default::default()
                        }))
                    })
                    .clone();
            }
            cop_stats
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .stats
                .merge_exec_summary(summary);
        }
        plan_id
    }

    /// `RecordOneCopTask` records a specific cop tasks's execution summary.
    // go: RuntimeStatsColl.RecordOneCopTask L1
    pub fn record_one_cop_task(
        &self,
        plan_id: i64,
        store_type: StoreType,
        summary: &ExecutorExecutionSummary,
    ) -> i64 {
        // for TiFlash cop response, ExecutorExecutionSummary contains executor id, so if there
        // is a valid executor id in summary, use it overwrite the planID
        let plan_id = get_plan_id_from_execution_summary(summary).unwrap_or(plan_id);
        let mut inner = self.lock();
        let cop_stats = inner
            .cop_stats
            .entry(plan_id)
            .or_insert_with(|| {
                Arc::new(Mutex::new(CopRuntimeStats {
                    store_type,
                    ..Default::default()
                }))
            })
            .clone();
        cop_stats
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .stats
            .merge_exec_summary(summary);
        plan_id
    }

    /// `ExistsRootStats` checks if the planID exists in the rootStats collection.
    // go: RuntimeStatsColl.ExistsRootStats L1
    pub fn exists_root_stats(&self, plan_id: i64) -> bool {
        self.lock().root_stats.contains_key(&plan_id)
    }

    /// `ExistsCopStats` checks if the planID exists in the copStats collection.
    // go: RuntimeStatsColl.ExistsCopStats L1
    pub fn exists_cop_stats(&self, plan_id: i64) -> bool {
        self.lock().cop_stats.contains_key(&plan_id)
    }
}

/// `getPlanIDFromExecutionSummary`: the number after the last `_` of the executor id
/// (`strconv.Atoi`).
// go: getPlanIDFromExecutionSummary L1
fn get_plan_id_from_execution_summary(summary: &ExecutorExecutionSummary) -> Option<i64> {
    let id = summary.get_executor_id();
    if !id.is_empty() {
        let last = id.rsplit('_').next().unwrap_or("");
        if let Some(v) = atoi(last) {
            return Some(v);
        }
    }
    None
}

/// `strconv.Atoi`: optional sign, decimal digits, int64 range (a 64-bit `int`).
// go-std: strconv.Atoi L1
fn atoi(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    let (neg, digits) = match b.first() {
        Some(b'+') => (false, &b[1..]),
        Some(b'-') => (true, &b[1..]),
        _ => (false, b),
    };
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let mut v: i128 = 0;
    for &c in digits {
        v = v * 10 + (c - b'0') as i128;
        if v > i64::MAX as i128 + 1 {
            return None;
        }
    }
    let v = if neg { -v } else { v };
    i64::try_from(v).ok()
}

/// `ConcurrencyInfo` is used to save the concurrency information of the executor operator.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConcurrencyInfo {
    concurrency_name: String,
    concurrency_num: i64,
}

impl ConcurrencyInfo {
    /// `NewConcurrencyInfo` creates new executor's concurrencyInfo.
    // go: NewConcurrencyInfo L1
    pub fn new(name: &str, num: i64) -> ConcurrencyInfo {
        ConcurrencyInfo {
            concurrency_name: name.to_string(),
            concurrency_num: num,
        }
    }
}

/// `RuntimeStatsWithConcurrencyInfo` is the BasicRuntimeStats with ConcurrencyInfo.
#[derive(Debug, Default)]
pub struct RuntimeStatsWithConcurrencyInfo {
    /// executor concurrency information (protected by the mutex in Go).
    concurrency: Mutex<Vec<ConcurrencyInfo>>,
}

impl RuntimeStatsWithConcurrencyInfo {
    /// `SetConcurrencyInfo` sets the concurrency informations. We must clear the
    /// concurrencyInfo first when we call the SetConcurrencyInfo. When the num <= 0, it means
    /// the exector operator is not executed parallel.
    // go: RuntimeStatsWithConcurrencyInfo.SetConcurrencyInfo L1
    pub fn set_concurrency_info(&self, infos: &[ConcurrencyInfo]) {
        let mut c = self.concurrency.lock().unwrap_or_else(|e| e.into_inner());
        c.clear();
        c.extend_from_slice(infos);
    }
}

impl RuntimeStats for RuntimeStatsWithConcurrencyInfo {
    /// `String` implements the RuntimeStats interface.
    // go: RuntimeStatsWithConcurrencyInfo.String L1
    fn string(&self) -> String {
        let c = self.concurrency.lock().unwrap_or_else(|e| e.into_inner());
        let mut buf = String::with_capacity(8);
        for (i, concurrency) in c.iter().enumerate() {
            if i > 0 {
                buf.push_str(", ");
            }
            if concurrency.concurrency_num > 0 {
                buf.push_str(&concurrency.concurrency_name);
                buf.push(':');
                buf.push_str(&concurrency.concurrency_num.to_string());
            } else {
                buf.push_str(&concurrency.concurrency_name);
                buf.push_str(":OFF");
            }
        }
        buf
    }
    /// `Merge` implements the RuntimeStats interface.
    // go: RuntimeStatsWithConcurrencyInfo.Merge L1
    fn merge(&mut self, _: &dyn RuntimeStats) {}
    /// `Clone` implements the RuntimeStats interface.
    // go: RuntimeStatsWithConcurrencyInfo.Clone L1
    fn clone_stats(&self) -> Box<dyn RuntimeStats> {
        let c = self.concurrency.lock().unwrap_or_else(|e| e.into_inner());
        Box::new(RuntimeStatsWithConcurrencyInfo {
            concurrency: Mutex::new(c.clone()),
        })
    }
    /// `Tp` implements the RuntimeStats interface.
    // go: RuntimeStatsWithConcurrencyInfo.Tp L1
    fn tp(&self) -> i64 {
        TpRuntimeStatsWithConcurrencyInfo
    }
    // glue: type assertion support
    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// `RuntimeStatsWithCommit` is the RuntimeStats with commit detail.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RuntimeStatsWithCommit {
    pub commit: Option<Box<CommitDetails>>,
    pub lock_keys: Option<Box<LockKeysDetails>>,
    pub shared_lock_keys: Option<Box<LockKeysDetails>>,
    pub txn_cnt: i64,
}

impl RuntimeStatsWithCommit {
    /// `MergeCommitDetails` merges the commit details. The first detail is stored as a copy
    /// (Go stores the pointer, block spec §4).
    // go: RuntimeStatsWithCommit.MergeCommitDetails L2 (detail is stored by value)
    pub fn merge_commit_details(&mut self, detail: Option<&CommitDetails>) {
        let Some(detail) = detail else { return };
        match &mut self.commit {
            None => {
                self.commit = Some(Box::new(detail.clone()));
                self.txn_cnt = 1;
            }
            Some(c) => {
                c.merge(detail);
                self.txn_cnt = self.txn_cnt.wrapping_add(1);
            }
        }
    }

    // go: RuntimeStatsWithCommit.formatBackoff L1
    fn format_backoff(buf: &mut String, backoff_types: &[String]) {
        if backoff_types.is_empty() {
            return;
        }
        let mut tp_array: Vec<&String> = Vec::new();
        for tp in backoff_types {
            if !tp_array.contains(&tp) {
                tp_array.push(tp);
            }
        }
        tp_array.sort();
        buf.push('[');
        for (i, tp) in tp_array.iter().enumerate() {
            if i > 0 {
                buf.push(' ');
            }
            buf.push_str(tp);
        }
        buf.push(']');
    }

    // go: RuntimeStatsWithCommit.formatLockKeysDetails L1
    fn format_lock_keys_details(
        buf: &mut String,
        label: &str,
        lock_keys: Option<&LockKeysDetails>,
    ) {
        let Some(lk) = lock_keys else { return };
        if !buf.is_empty() {
            buf.push_str(", ");
        }
        buf.push_str(label);
        buf.push_str(": {");
        if lk.total_time > 0 {
            buf.push_str("time:");
            buf.push_str(&format_duration(lk.total_time));
        }
        if lk.region_num > 0 {
            buf.push_str(", region:");
            buf.push_str(&lk.region_num.to_string());
        }
        if lk.lock_keys > 0 {
            buf.push_str(", keys:");
            buf.push_str(&lk.lock_keys.to_string());
        }
        if lk.resolve_lock.resolve_lock_time > 0 {
            buf.push_str(", resolve_lock:");
            buf.push_str(&format_duration(lk.resolve_lock.resolve_lock_time));
        }
        if lk.backoff_time > 0 {
            buf.push_str(", backoff: {time: ");
            buf.push_str(&format_duration(lk.backoff_time));
            if !lk.mu.backoff_types.is_empty() {
                buf.push_str(", type: ");
                Self::format_backoff(buf, &lk.mu.backoff_types);
            }
            buf.push('}');
        }
        if lk.mu.slowest_req_total_time > 0 {
            buf.push_str(", slowest_rpc: {total: ");
            buf.push_str(&secs(lk.mu.slowest_req_total_time, 3));
            buf.push_str("s, region_id: ");
            buf.push_str(&lk.mu.slowest_region.to_string());
            buf.push_str(", store: ");
            buf.push_str(&lk.mu.slowest_store_addr);
            buf.push_str(", ");
            buf.push_str(&lk.mu.slowest_exec_details.string());
            buf.push('}');
        }
        if lk.lock_rpc_time > 0 {
            buf.push_str(", lock_rpc:");
            buf.push_str(&duration_string(lk.lock_rpc_time));
        }
        if lk.lock_rpc_count > 0 {
            buf.push_str(", rpc_count:");
            buf.push_str(&lk.lock_rpc_count.to_string());
        }
        if lk.retry_count > 0 {
            buf.push_str(", retry_count:");
            buf.push_str(&lk.retry_count.to_string());
        }
        buf.push('}');
    }
}

impl RuntimeStats for RuntimeStatsWithCommit {
    /// `String` implements the RuntimeStats interface.
    // go: RuntimeStatsWithCommit.String L1
    fn string(&self) -> String {
        let mut buf = String::with_capacity(32);
        if let Some(c) = &self.commit {
            buf.push_str("commit_txn: {");
            // Only print out when there are more than 1 transaction.
            if self.txn_cnt > 1 {
                buf.push_str("count: ");
                buf.push_str(&self.txn_cnt.to_string());
                buf.push_str(", ");
            }
            if c.prewrite_time > 0 {
                buf.push_str("prewrite:");
                buf.push_str(&format_duration(c.prewrite_time));
            }
            if c.wait_prewrite_binlog_time > 0 {
                buf.push_str(", wait_prewrite_binlog:");
                buf.push_str(&format_duration(c.wait_prewrite_binlog_time));
            }
            if c.get_commit_ts_time > 0 {
                buf.push_str(", get_commit_ts:");
                buf.push_str(&format_duration(c.get_commit_ts_time));
            }
            if c.commit_time > 0 {
                buf.push_str(", commit:");
                buf.push_str(&format_duration(c.commit_time));
            }
            let commit_backoff_time = c.mu.commit_backoff_time;
            if commit_backoff_time > 0 {
                buf.push_str(", backoff: {time: ");
                buf.push_str(&format_duration(commit_backoff_time));
                if !c.mu.prewrite_backoff_types.is_empty() {
                    buf.push_str(", prewrite type: ");
                    Self::format_backoff(&mut buf, &c.mu.prewrite_backoff_types);
                }
                if !c.mu.commit_backoff_types.is_empty() {
                    buf.push_str(", commit type: ");
                    Self::format_backoff(&mut buf, &c.mu.commit_backoff_types);
                }
                buf.push('}');
            }
            for (label, r) in [
                (", slowest_prewrite_rpc: {total: ", &c.mu.slowest_prewrite),
                (", commit_primary_rpc: {total: ", &c.mu.commit_primary),
            ] {
                if r.req_total_time > 0 {
                    buf.push_str(label);
                    buf.push_str(&secs(r.req_total_time, 3));
                    buf.push_str("s, region_id: ");
                    buf.push_str(&r.region.to_string());
                    buf.push_str(", store: ");
                    buf.push_str(&r.store_addr);
                    buf.push_str(", ");
                    buf.push_str(&r.exec_details.string());
                    buf.push('}');
                }
            }
            if c.resolve_lock.resolve_lock_time > 0 {
                buf.push_str(", resolve_lock: ");
                buf.push_str(&format_duration(c.resolve_lock.resolve_lock_time));
            }

            let prewrite_region_num = c.prewrite_region_num;
            if prewrite_region_num > 0 {
                buf.push_str(", region_num:");
                buf.push_str(&prewrite_region_num.to_string());
            }
            if c.write_keys > 0 {
                buf.push_str(", write_keys:");
                buf.push_str(&c.write_keys.to_string());
            }
            if c.write_size > 0 {
                buf.push_str(", write_byte:");
                buf.push_str(&c.write_size.to_string());
            }
            if c.txn_retry > 0 {
                buf.push_str(", txn_retry:");
                buf.push_str(&c.txn_retry.to_string());
            }
            buf.push('}');
        }
        Self::format_lock_keys_details(&mut buf, "lock_keys", self.lock_keys.as_deref());
        Self::format_lock_keys_details(
            &mut buf,
            "shared_lock_keys",
            self.shared_lock_keys.as_deref(),
        );
        buf
    }

    /// `Merge` implements the RuntimeStats interface.
    // go: RuntimeStatsWithCommit.Merge L1
    fn merge(&mut self, rs: &dyn RuntimeStats) {
        let Some(tmp) = rs.as_any().downcast_ref::<RuntimeStatsWithCommit>() else {
            return;
        };
        self.txn_cnt = self.txn_cnt.wrapping_add(tmp.txn_cnt);
        if let Some(c) = &tmp.commit {
            self.commit.get_or_insert_with(Default::default).merge(c);
        }
        if let Some(l) = &tmp.lock_keys {
            self.lock_keys.get_or_insert_with(Default::default).merge(l);
        }
        if let Some(l) = &tmp.shared_lock_keys {
            self.shared_lock_keys
                .get_or_insert_with(Default::default)
                .merge(l);
        }
    }

    /// `Clone` implements the RuntimeStats interface.
    // go: RuntimeStatsWithCommit.Clone L1
    fn clone_stats(&self) -> Box<dyn RuntimeStats> {
        Box::new(RuntimeStatsWithCommit {
            txn_cnt: self.txn_cnt,
            commit: self.commit.as_ref().map(|c| Box::new(c.clone_details())),
            lock_keys: self.lock_keys.as_ref().map(|l| Box::new(l.clone_details())),
            shared_lock_keys: self
                .shared_lock_keys
                .as_ref()
                .map(|l| Box::new(l.clone_details())),
        })
    }

    /// `Tp` implements the RuntimeStats interface.
    // go: RuntimeStatsWithCommit.Tp L1
    fn tp(&self) -> i64 {
        TpRuntimeStatsWithCommit
    }

    // glue: type assertion support
    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// `FormatDuration` uses to format duration, this function will prune precision before
/// format duration. Pruning precision is for human readability. The prune rule is:
///  1. if the duration was less than 1us, return the original string.
///  2. readable value >=10, keep 1 decimal, otherwise, keep 2 decimal. such as:
///     9.412345ms  -> 9.41ms
///     10.412345ms -> 10.4ms
///     5.999s      -> 6s
///     100.45µs    -> 100.5µs
// go: FormatDuration L1
pub fn format_duration(d: Duration) -> String {
    if d <= MICROSECOND {
        return duration_string(d);
    }
    let unit = get_unit(d);
    if unit == NANOSECOND {
        return duration_string(d);
    }
    let integer = (d / unit) * unit;
    let mut decimal = (d % unit) as f64 / unit as f64;
    if d < 10 * unit {
        decimal = (decimal * 100.0).round() / 100.0;
    } else {
        decimal = (decimal * 10.0).round() / 10.0;
    }
    let d = integer.wrapping_add(f64_to_i64(decimal * unit as f64)); // [M-F2I]
    duration_string(d)
}

// go: getUnit L1
fn get_unit(d: Duration) -> Duration {
    if d >= SECOND {
        SECOND
    } else if d >= MILLISECOND {
        MILLISECOND
    } else if d >= MICROSECOND {
        MICROSECOND
    } else {
        NANOSECOND
    }
}

/// `RURuntimeStats` is a wrapper of util.RUDetails, which implements the RuntimeStats
/// interface. Go's nil `*util.RUDetails` is `None`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RURuntimeStats {
    pub ru_details: Option<RUDetails>,
}

impl RuntimeStats for RURuntimeStats {
    /// `String` implements the RuntimeStats interface.
    // go: RURuntimeStats.String L1
    fn string(&self) -> String {
        match &self.ru_details {
            Some(d) => format!("RU:{}", format_float(d.rru() + d.wru(), b'f', 2, 64)),
            None => String::new(),
        }
    }

    /// `Clone` implements the RuntimeStats interface (a nil `RUDetails` dereference panics, as
    /// in Go).
    // go: RURuntimeStats.Clone L1
    fn clone_stats(&self) -> Box<dyn RuntimeStats> {
        let d = self
            .ru_details
            .as_ref()
            .expect("runtime error: invalid memory address or nil pointer dereference");
        Box::new(RURuntimeStats {
            ru_details: Some(d.clone_details()),
        })
    }

    /// `Merge` implements the RuntimeStats interface (a nil `RUDetails` in `other` panics, as
    /// in Go).
    // go: RURuntimeStats.Merge L1
    fn merge(&mut self, other: &dyn RuntimeStats) {
        if let Some(tmp) = other.as_any().downcast_ref::<RURuntimeStats>() {
            let o = tmp
                .ru_details
                .as_ref()
                .expect("runtime error: invalid memory address or nil pointer dereference");
            match &mut self.ru_details {
                Some(d) => d.merge(o),
                None => self.ru_details = Some(o.clone_details()),
            }
        }
    }

    /// `Tp` implements the RuntimeStats interface.
    // go: RURuntimeStats.Tp L1
    fn tp(&self) -> i64 {
        TpRURuntimeStats
    }

    // glue: type assertion support
    fn as_any(&self) -> &dyn Any {
        self
    }
}
