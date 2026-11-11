//! go-dep-file: github.com/tikv/client-go/v2@v2.0.8-0.20260803075849-c3b50791b9fb util/execdetails.go, util/misc.go
//!
//! The client-go `util` execution-detail types that `pkg/util/execdetails` embeds and
//! prints (L1 port of the dependency, only what TiDB's execdetails uses). They carry what
//! the storage client measured; tidb-rs's client (client-rust, ADR 0007 / L3) fills them in
//! D07.1/D07.2, the slow log, `EXPLAIN ANALYZE` and the statement summary print them with
//! these exact formats.
//!
//! Mapping:
//! - `time.Duration` is `i64` nanoseconds ([`Duration`]).
//! - The `Mu struct { sync.Mutex; ... }` of [`CommitDetails`] and [`LockKeysDetails`] is a
//!   plain field: Go locks it inside `String` / `MergeReqDetails`; in Rust the owner that
//!   shares a detail between tasks wraps it in a mutex (M-SYNC), and `&mut self` / `&self`
//!   give the same exclusion.
//! - `atomic.AddInt64` in `Merge` is a plain `+=` under `&mut self` (Go's wrap-around is
//!   reproduced with wrapping arithmetic).
//! - `*TimeDetail` / `*ScanDetail` / `*WriteDetail` in [`TiKVExecDetails`] are shared,
//!   read-only pointers: `Option<Arc<..>>`.
//! - `RUDetails.Update` (TiFlash RU consumption, kvproto `resource_manager.Consumption`) is
//!   not ported with `MergeTiFlashRUConsumption` (block spec §4).
//! - [`RUDetails`]'s `*uatomic.Float64` / `*uatomic.Duration` are plain values; `Clone` is a
//!   deep copy, as Go's.

use std::sync::Arc;

use tidb_base::gostd::strconv::format_float;
use tidb_base::gostd::time::{Duration, duration_string};

/// `TiKVExecDetails` contains detailed information about TiKV execution.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TiKVExecDetails {
    pub time_detail: Option<Arc<TimeDetail>>,
    pub scan_detail: Option<Arc<ScanDetail>>,
    pub write_detail: Option<Arc<WriteDetail>>,
}

impl TiKVExecDetails {
    // go-dep: github.com/tikv/client-go/v2@v2.0.8-0.20260803075849-c3b50791b9fb.TiKVExecDetails.String L1
    pub fn string(&self) -> String {
        let mut buf = String::with_capacity(16);
        if let Some(td) = &self.time_detail {
            buf.push_str(&td.string());
        }
        if let Some(sd) = &self.scan_detail {
            if !buf.is_empty() {
                buf.push_str(", ");
            }
            buf.push_str(&sd.string());
        }
        if let Some(wd) = &self.write_detail {
            if !buf.is_empty() {
                buf.push_str(", ");
            }
            buf.push_str(&wd.string());
        }
        buf
    }
}

/// `ReqDetailInfo` contains diagnose information about `TiKVExecDetails`, region, store and
/// backoff.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ReqDetailInfo {
    pub req_total_time: Duration,
    pub region: u64,
    pub store_addr: String,
    pub exec_details: TiKVExecDetails,
}

/// `CommitTSLagDetails` contain the detail when the commit timestamp from PD lags the
/// expected ts set by `SetCommitWaitUntilTSO`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CommitTSLagDetails {
    pub wait_time: Duration,
    pub backoff_cnt: i64,
    pub first_lag_ts: u64,
    pub wait_until_ts: u64,
}

impl CommitTSLagDetails {
    /// `Merge` merges CommitTSLagDetails with another one.
    // go-dep: github.com/tikv/client-go/v2@v2.0.8-0.20260803075849-c3b50791b9fb.CommitTSLagDetails.Merge L1
    pub fn merge(&mut self, other: Option<&CommitTSLagDetails>) {
        let Some(other) = other else { return };
        if other.first_lag_ts == 0 {
            // other.FirstLagTS <= 0 indicates no lag happen, do not need to merge the new details.
            return;
        }
        self.wait_time = self.wait_time.wrapping_add(other.wait_time);
        self.backoff_cnt = self.backoff_cnt.wrapping_add(other.backoff_cnt);
        // For sample, we use the last lag timestamps after merge
        self.first_lag_ts = other.first_lag_ts;
        self.wait_until_ts = other.wait_until_ts;
    }
}

/// The `Mu` part of [`CommitDetails`] (Go: guarded by `Mu.Mutex`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CommitDetailsMu {
    /// The total backoff time used in both the prewrite and commit phases.
    pub commit_backoff_time: i64,
    pub prewrite_backoff_types: Vec<String>,
    pub commit_backoff_types: Vec<String>,
    /// The prewrite requests are executed concurrently so the slowest request information
    /// would be recorded.
    pub slowest_prewrite: ReqDetailInfo,
    /// It's recorded only when the commit mode is 2pc.
    pub commit_primary: ReqDetailInfo,
}

/// `CommitDetails` contains commit detail information.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CommitDetails {
    pub get_commit_ts_time: Duration,
    pub get_latest_ts_time: Duration,
    pub lag_details: CommitTSLagDetails,
    pub prewrite_time: Duration,
    pub wait_prewrite_binlog_time: Duration,
    pub commit_time: Duration,
    pub local_latch_time: Duration,
    pub mu: CommitDetailsMu,
    pub write_keys: i64,
    pub write_size: i64,
    pub prewrite_region_num: i32,
    pub txn_retry: i64,
    pub resolve_lock: ResolveLockDetail,
    pub prewrite_req_num: i64,
}

impl CommitDetails {
    /// `Merge` merges commit details into itself.
    // go-dep: github.com/tikv/client-go/v2@v2.0.8-0.20260803075849-c3b50791b9fb.CommitDetails.Merge L1
    pub fn merge(&mut self, other: &CommitDetails) {
        self.get_commit_ts_time = self
            .get_commit_ts_time
            .wrapping_add(other.get_commit_ts_time);
        self.get_latest_ts_time = self
            .get_latest_ts_time
            .wrapping_add(other.get_latest_ts_time);
        self.prewrite_time = self.prewrite_time.wrapping_add(other.prewrite_time);
        self.lag_details.merge(Some(&other.lag_details));
        self.wait_prewrite_binlog_time = self
            .wait_prewrite_binlog_time
            .wrapping_add(other.wait_prewrite_binlog_time);
        self.commit_time = self.commit_time.wrapping_add(other.commit_time);
        self.local_latch_time = self.local_latch_time.wrapping_add(other.local_latch_time);
        self.resolve_lock.resolve_lock_time = self
            .resolve_lock
            .resolve_lock_time
            .wrapping_add(other.resolve_lock.resolve_lock_time);
        self.write_keys = self.write_keys.wrapping_add(other.write_keys);
        self.write_size = self.write_size.wrapping_add(other.write_size);
        self.prewrite_region_num = self
            .prewrite_region_num
            .wrapping_add(other.prewrite_region_num);
        self.txn_retry = self.txn_retry.wrapping_add(other.txn_retry);
        self.mu.commit_backoff_time = self
            .mu
            .commit_backoff_time
            .wrapping_add(other.mu.commit_backoff_time);

        self.mu
            .prewrite_backoff_types
            .extend_from_slice(&other.mu.prewrite_backoff_types);
        if self.mu.slowest_prewrite.req_total_time < other.mu.slowest_prewrite.req_total_time {
            self.mu.slowest_prewrite = other.mu.slowest_prewrite.clone();
        }

        self.mu
            .commit_backoff_types
            .extend_from_slice(&other.mu.commit_backoff_types);
        if self.mu.commit_primary.req_total_time < other.mu.commit_primary.req_total_time {
            self.mu.commit_primary = other.mu.commit_primary.clone();
        }
    }

    /// `Clone` returns a deep copy of itself. `PrewriteReqNum` is not copied (as in Go).
    // go-dep: github.com/tikv/client-go/v2@v2.0.8-0.20260803075849-c3b50791b9fb.CommitDetails.Clone L1
    pub fn clone_details(&self) -> CommitDetails {
        CommitDetails {
            get_commit_ts_time: self.get_commit_ts_time,
            get_latest_ts_time: self.get_latest_ts_time,
            lag_details: self.lag_details,
            prewrite_time: self.prewrite_time,
            wait_prewrite_binlog_time: self.wait_prewrite_binlog_time,
            commit_time: self.commit_time,
            local_latch_time: self.local_latch_time,
            write_keys: self.write_keys,
            write_size: self.write_size,
            prewrite_region_num: self.prewrite_region_num,
            txn_retry: self.txn_retry,
            resolve_lock: self.resolve_lock,
            mu: CommitDetailsMu {
                commit_backoff_time: self.mu.commit_backoff_time,
                prewrite_backoff_types: self.mu.prewrite_backoff_types.clone(),
                commit_backoff_types: self.mu.commit_backoff_types.clone(),
                slowest_prewrite: self.mu.slowest_prewrite.clone(),
                commit_primary: self.mu.commit_primary.clone(),
            },
            prewrite_req_num: 0,
        }
    }
}

/// The `Mu` part of [`LockKeysDetails`] (Go: guarded by `Mu.Mutex`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LockKeysDetailsMu {
    pub backoff_types: Vec<String>,
    pub slowest_req_total_time: Duration,
    pub slowest_region: u64,
    pub slowest_store_addr: String,
    pub slowest_exec_details: TiKVExecDetails,
}

/// `LockKeysDetails` contains pessimistic lock keys detail information.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LockKeysDetails {
    pub total_time: Duration,
    pub region_num: i32,
    pub lock_keys: i32,
    pub aggressive_lock_new_count: i64,
    pub aggressive_lock_derived_count: i64,
    pub locked_with_conflict_count: i64,
    pub resolve_lock: ResolveLockDetail,
    pub backoff_time: i64,
    pub mu: LockKeysDetailsMu,
    pub lock_rpc_time: i64,
    pub lock_rpc_count: i64,
    pub retry_count: i64,
}

impl LockKeysDetails {
    /// `Merge` merges lock keys execution details into self.
    // go-dep: github.com/tikv/client-go/v2@v2.0.8-0.20260803075849-c3b50791b9fb.LockKeysDetails.Merge L1
    pub fn merge(&mut self, lock_key: &LockKeysDetails) {
        self.total_time = self.total_time.wrapping_add(lock_key.total_time);
        self.region_num = self.region_num.wrapping_add(lock_key.region_num);
        self.lock_keys = self.lock_keys.wrapping_add(lock_key.lock_keys);
        self.aggressive_lock_new_count = self
            .aggressive_lock_new_count
            .wrapping_add(lock_key.aggressive_lock_new_count);
        self.aggressive_lock_derived_count = self
            .aggressive_lock_derived_count
            .wrapping_add(lock_key.aggressive_lock_derived_count);
        self.locked_with_conflict_count = self
            .locked_with_conflict_count
            .wrapping_add(lock_key.locked_with_conflict_count);
        self.resolve_lock.resolve_lock_time = self
            .resolve_lock
            .resolve_lock_time
            .wrapping_add(lock_key.resolve_lock.resolve_lock_time);
        self.backoff_time = self.backoff_time.wrapping_add(lock_key.backoff_time);
        self.lock_rpc_time = self.lock_rpc_time.wrapping_add(lock_key.lock_rpc_time);
        self.lock_rpc_count = self.lock_rpc_count.wrapping_add(lock_key.lock_rpc_count);
        self.mu
            .backoff_types
            .extend_from_slice(&lock_key.mu.backoff_types);
        self.retry_count = self.retry_count.wrapping_add(1);
        if self.mu.slowest_req_total_time < lock_key.mu.slowest_req_total_time {
            self.mu.slowest_req_total_time = lock_key.mu.slowest_req_total_time;
            self.mu.slowest_region = lock_key.mu.slowest_region;
            self.mu.slowest_store_addr = lock_key.mu.slowest_store_addr.clone();
            self.mu.slowest_exec_details = lock_key.mu.slowest_exec_details.clone();
        }
    }

    /// `Clone` returns a deep copy of itself.
    // go-dep: github.com/tikv/client-go/v2@v2.0.8-0.20260803075849-c3b50791b9fb.LockKeysDetails.Clone L1
    pub fn clone_details(&self) -> LockKeysDetails {
        self.clone()
    }
}

/// `FormatDuration` uses to format duration, this function will prune precision before
/// format duration (the same as `execdetails.FormatDuration`).
// go-dep: github.com/tikv/client-go/v2@v2.0.8-0.20260803075849-c3b50791b9fb.FormatDuration L1
pub fn format_duration(d: Duration) -> String {
    crate::execdetails::format_duration(d)
}

/// `ScanDetail` contains coprocessor scan detail information.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScanDetail {
    /// TotalKeys is the approximate number of MVCC keys meet during scanning.
    pub total_keys: i64,
    /// ProcessedKeys is the number of user keys scanned from the storage.
    pub processed_keys: i64,
    /// Number of bytes of user key-value pairs scanned from the storage.
    pub processed_keys_size: i64,
    pub rocksdb_delete_skipped_count: u64,
    pub rocksdb_key_skipped_count: u64,
    pub rocksdb_block_cache_hit_count: u64,
    pub rocksdb_block_read_count: u64,
    pub rocksdb_block_read_byte: u64,
    pub rocksdb_block_read_duration: Duration,
    pub get_snapshot_duration: Duration,
}

impl ScanDetail {
    /// `Merge` merges scan detail execution details into self.
    // go-dep: github.com/tikv/client-go/v2@v2.0.8-0.20260803075849-c3b50791b9fb.ScanDetail.Merge L1
    pub fn merge(&mut self, scan_detail: &ScanDetail) {
        self.total_keys = self.total_keys.wrapping_add(scan_detail.total_keys);
        self.processed_keys = self.processed_keys.wrapping_add(scan_detail.processed_keys);
        self.processed_keys_size = self
            .processed_keys_size
            .wrapping_add(scan_detail.processed_keys_size);
        self.rocksdb_delete_skipped_count = self
            .rocksdb_delete_skipped_count
            .wrapping_add(scan_detail.rocksdb_delete_skipped_count);
        self.rocksdb_key_skipped_count = self
            .rocksdb_key_skipped_count
            .wrapping_add(scan_detail.rocksdb_key_skipped_count);
        self.rocksdb_block_cache_hit_count = self
            .rocksdb_block_cache_hit_count
            .wrapping_add(scan_detail.rocksdb_block_cache_hit_count);
        self.rocksdb_block_read_count = self
            .rocksdb_block_read_count
            .wrapping_add(scan_detail.rocksdb_block_read_count);
        self.rocksdb_block_read_byte = self
            .rocksdb_block_read_byte
            .wrapping_add(scan_detail.rocksdb_block_read_byte);
        self.rocksdb_block_read_duration = self
            .rocksdb_block_read_duration
            .wrapping_add(scan_detail.rocksdb_block_read_duration);
        self.get_snapshot_duration = self
            .get_snapshot_duration
            .wrapping_add(scan_detail.get_snapshot_duration);
    }

    /// `String` implements the fmt.Stringer interface (a nil detail prints "").
    // go-dep: github.com/tikv/client-go/v2@v2.0.8-0.20260803075849-c3b50791b9fb.ScanDetail.String L1
    pub fn string(&self) -> String {
        if *self == ScanDetail::default() {
            return String::new();
        }
        let mut buf = String::with_capacity(16);
        buf.push_str("scan_detail: {");
        if self.processed_keys > 0 {
            buf.push_str("total_process_keys: ");
            buf.push_str(&self.processed_keys.to_string());
            buf.push_str(", ");
        }
        if self.processed_keys_size > 0 {
            buf.push_str("total_process_keys_size: ");
            buf.push_str(&self.processed_keys_size.to_string());
            buf.push_str(", ");
        }
        if self.total_keys > 0 {
            buf.push_str("total_keys: ");
            buf.push_str(&self.total_keys.to_string());
            buf.push_str(", ");
        }
        if self.get_snapshot_duration > 0 {
            buf.push_str("get_snapshot_time: ");
            buf.push_str(&format_duration(self.get_snapshot_duration));
            buf.push_str(", ");
        }
        buf.push_str("rocksdb: {");
        if self.rocksdb_delete_skipped_count > 0 {
            buf.push_str("delete_skipped_count: ");
            buf.push_str(&self.rocksdb_delete_skipped_count.to_string());
            buf.push_str(", ");
        }
        if self.rocksdb_key_skipped_count > 0 {
            buf.push_str("key_skipped_count: ");
            buf.push_str(&self.rocksdb_key_skipped_count.to_string());
            buf.push_str(", ");
        }
        buf.push_str("block: {");
        if self.rocksdb_block_cache_hit_count > 0 {
            buf.push_str("cache_hit_count: ");
            buf.push_str(&self.rocksdb_block_cache_hit_count.to_string());
            buf.push_str(", ");
        }
        if self.rocksdb_block_read_count > 0 {
            buf.push_str("read_count: ");
            buf.push_str(&self.rocksdb_block_read_count.to_string());
            buf.push_str(", ");
        }
        if self.rocksdb_block_read_byte > 0 {
            buf.push_str("read_byte: ");
            buf.push_str(&format_bytes(self.rocksdb_block_read_byte as i64));
            buf.push_str(", ");
        }
        if self.rocksdb_block_read_duration > 0 {
            buf.push_str("read_time: ");
            buf.push_str(&format_duration(self.rocksdb_block_read_duration));
        }
        if buf.as_bytes()[buf.len() - 2] == b',' {
            buf.truncate(buf.len() - 2);
        }
        buf.push_str("}}}");
        buf
    }
}

/// `WriteDetail` contains the detailed time breakdown of a write operation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WriteDetail {
    pub store_batch_wait_duration: Duration,
    pub propose_send_wait_duration: Duration,
    pub persist_log_duration: Duration,
    pub raft_db_write_leader_wait_duration: Duration,
    pub raft_db_sync_log_duration: Duration,
    pub raft_db_write_memtable_duration: Duration,
    pub commit_log_duration: Duration,
    pub apply_batch_wait_duration: Duration,
    pub apply_log_duration: Duration,
    pub apply_mutex_lock_duration: Duration,
    pub apply_write_leader_wait_duration: Duration,
    pub apply_write_wal_duration: Duration,
    pub apply_write_memtable_duration: Duration,
}

impl WriteDetail {
    // go-dep: github.com/tikv/client-go/v2@v2.0.8-0.20260803075849-c3b50791b9fb.WriteDetail.String L1
    pub fn string(&self) -> String {
        if *self == WriteDetail::default() {
            return String::new();
        }
        let f = format_duration;
        let mut buf = String::with_capacity(64);
        buf.push_str("write_detail: {");
        buf.push_str("store_batch_wait: ");
        buf.push_str(&f(self.store_batch_wait_duration));
        buf.push_str(", propose_send_wait: ");
        buf.push_str(&f(self.propose_send_wait_duration));
        buf.push_str(", persist_log: {total: ");
        buf.push_str(&f(self.persist_log_duration));
        buf.push_str(", write_leader_wait: ");
        buf.push_str(&f(self.raft_db_write_leader_wait_duration));
        buf.push_str(", sync_log: ");
        buf.push_str(&f(self.raft_db_sync_log_duration));
        buf.push_str(", write_memtable: ");
        buf.push_str(&f(self.raft_db_write_memtable_duration));
        buf.push_str("}, commit_log: ");
        buf.push_str(&f(self.commit_log_duration));
        buf.push_str(", apply_batch_wait: ");
        buf.push_str(&f(self.apply_batch_wait_duration));
        buf.push_str(", apply: {total:");
        buf.push_str(&f(self.apply_log_duration));
        buf.push_str(", mutex_lock: ");
        buf.push_str(&f(self.apply_mutex_lock_duration));
        buf.push_str(", write_leader_wait: ");
        buf.push_str(&f(self.apply_write_leader_wait_duration));
        buf.push_str(", write_wal: ");
        buf.push_str(&f(self.apply_write_wal_duration));
        buf.push_str(", write_memtable: ");
        buf.push_str(&f(self.apply_write_memtable_duration));
        buf.push_str("}}");
        buf
    }
}

/// `TimeDetail` contains coprocessor time detail information.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TimeDetail {
    /// Off-cpu and on-cpu wall time elapsed to actually process the request payload.
    pub process_time: Duration,
    /// Time elapsed when a coprocessor task yields itself.
    pub suspend_time: Duration,
    /// Off-cpu wall time elapsed in TiKV side.
    pub wait_time: Duration,
    /// KvReadWallTime is the time used in KV Scan/Get.
    pub kv_read_wall_time: Duration,
    /// TotalRPCWallTime is Total wall clock time spent on this RPC in TiKV.
    pub total_rpc_wall_time: Duration,
}

impl TimeDetail {
    /// `String` implements the fmt.Stringer interface.
    // go-dep: github.com/tikv/client-go/v2@v2.0.8-0.20260803075849-c3b50791b9fb.TimeDetail.String L1
    pub fn string(&self) -> String {
        let mut buf = String::with_capacity(16);
        let items = [
            (self.process_time, "total_process_time: "),
            (self.suspend_time, "total_suspend_time: "),
            (self.wait_time, "total_wait_time: "),
            (self.kv_read_wall_time, "total_kv_read_wall_time: "),
            (self.total_rpc_wall_time, "tikv_wall_time: "),
        ];
        for (d, label) in items {
            if d > 0 {
                if !buf.is_empty() {
                    buf.push_str(", ");
                }
                buf.push_str(label);
                buf.push_str(&format_duration(d));
            }
        }
        if buf.is_empty() {
            return String::new();
        }
        format!("time_detail: {{{buf}}}")
    }

    /// `Merge` merges the time detail into itself.
    // go-dep: github.com/tikv/client-go/v2@v2.0.8-0.20260803075849-c3b50791b9fb.TimeDetail.Merge L1
    pub fn merge(&mut self, detail: Option<&TimeDetail>) {
        if let Some(d) = detail {
            self.process_time = self.process_time.wrapping_add(d.process_time);
            self.suspend_time = self.suspend_time.wrapping_add(d.suspend_time);
            self.wait_time = self.wait_time.wrapping_add(d.wait_time);
            self.kv_read_wall_time = self.kv_read_wall_time.wrapping_add(d.kv_read_wall_time);
            self.total_rpc_wall_time = self.total_rpc_wall_time.wrapping_add(d.total_rpc_wall_time);
        }
    }
}

/// `ResolveLockDetail` contains the resolve lock detail information.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ResolveLockDetail {
    /// ResolveLockTime is the total duration of resolving lock.
    pub resolve_lock_time: i64,
}

impl ResolveLockDetail {
    /// `Merge` merges resolve lock detail details into self.
    // go-dep: github.com/tikv/client-go/v2@v2.0.8-0.20260803075849-c3b50791b9fb.ResolveLockDetail.Merge L1
    pub fn merge(&mut self, resolve_lock: &ResolveLockDetail) {
        self.resolve_lock_time = self
            .resolve_lock_time
            .wrapping_add(resolve_lock.resolve_lock_time);
    }
}

/// `RUDetails` contains RU detail info.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RUDetails {
    read_ru: f64,
    write_ru: f64,
    ru_wait_duration: Duration,
}

impl RUDetails {
    /// `NewRUDetails` creates a new RUDetails.
    // go-dep: github.com/tikv/client-go/v2@v2.0.8-0.20260803075849-c3b50791b9fb.NewRUDetails L1
    pub fn new() -> RUDetails {
        RUDetails::default()
    }

    /// `NewRUDetailsWith` creates a new RUDetails with specifical values.
    // go-dep: github.com/tikv/client-go/v2@v2.0.8-0.20260803075849-c3b50791b9fb.NewRUDetailsWith L1
    pub fn new_with(rru: f64, wru: f64, wait_dur: Duration) -> RUDetails {
        RUDetails {
            read_ru: rru,
            write_ru: wru,
            ru_wait_duration: wait_dur,
        }
    }

    /// `Clone`: a deep copy.
    // go-dep: github.com/tikv/client-go/v2@v2.0.8-0.20260803075849-c3b50791b9fb.RUDetails.Clone L1
    pub fn clone_details(&self) -> RUDetails {
        *self
    }

    /// `Merge` adds other's values.
    // go-dep: github.com/tikv/client-go/v2@v2.0.8-0.20260803075849-c3b50791b9fb.RUDetails.Merge L1
    pub fn merge(&mut self, other: &RUDetails) {
        self.read_ru += other.read_ru;
        self.write_ru += other.write_ru;
        self.ru_wait_duration = self.ru_wait_duration.wrapping_add(other.ru_wait_duration);
    }

    /// `String`: `RRU:%f, WRU:%f, WaitDuration:%v`.
    // go-dep: github.com/tikv/client-go/v2@v2.0.8-0.20260803075849-c3b50791b9fb.RUDetails.String L1
    pub fn string(&self) -> String {
        format!(
            "RRU:{}, WRU:{}, WaitDuration:{}",
            format_float(self.read_ru, b'f', 6, 64),
            format_float(self.write_ru, b'f', 6, 64),
            duration_string(self.ru_wait_duration)
        )
    }

    /// `RRU` returns the read RU.
    // go-dep: github.com/tikv/client-go/v2@v2.0.8-0.20260803075849-c3b50791b9fb.RUDetails.RRU L1
    pub fn rru(&self) -> f64 {
        self.read_ru
    }

    /// `WRU` returns the write RU.
    // go-dep: github.com/tikv/client-go/v2@v2.0.8-0.20260803075849-c3b50791b9fb.RUDetails.WRU L1
    pub fn wru(&self) -> f64 {
        self.write_ru
    }

    /// `RUWaitDuration` returns the time duration waiting for available RU.
    // go-dep: github.com/tikv/client-go/v2@v2.0.8-0.20260803075849-c3b50791b9fb.RUDetails.RUWaitDuration L1
    pub fn ru_wait_duration(&self) -> Duration {
        self.ru_wait_duration
    }
}

const BYTE_SIZE_GB: i64 = 1 << 30;
const BYTE_SIZE_MB: i64 = 1 << 20;
const BYTE_SIZE_KB: i64 = 1 << 10;
const BYTE_SIZE_BB: i64 = 1;

/// `FormatBytes` uses to format bytes, this function will prune precision before format
/// bytes.
// go-dep: github.com/tikv/client-go/v2@v2.0.8-0.20260803075849-c3b50791b9fb.FormatBytes L1
pub fn format_bytes(num_bytes: i64) -> String {
    if num_bytes <= BYTE_SIZE_KB {
        return bytes_to_string(num_bytes);
    }
    let (unit, unit_str) = get_byte_unit(num_bytes);
    if unit == BYTE_SIZE_BB {
        return bytes_to_string(num_bytes);
    }
    let v = num_bytes as f64 / unit as f64;
    let mut decimal = 1;
    if num_bytes % unit == 0 {
        decimal = 0;
    } else if v < 10.0 {
        decimal = 2;
    }
    format!("{} {}", format_float(v, b'f', decimal, 64), unit_str)
}

// go-dep: github.com/tikv/client-go/v2@v2.0.8-0.20260803075849-c3b50791b9fb.getByteUnit L1
fn get_byte_unit(b: i64) -> (i64, &'static str) {
    if b > BYTE_SIZE_GB {
        (BYTE_SIZE_GB, "GB")
    } else if b > BYTE_SIZE_MB {
        (BYTE_SIZE_MB, "MB")
    } else if b > BYTE_SIZE_KB {
        (BYTE_SIZE_KB, "KB")
    } else {
        (BYTE_SIZE_BB, "Bytes")
    }
}

/// `BytesToString` converts the memory consumption to a readable string (`%v` of a float64
/// is `strconv.FormatFloat(f, 'g', -1, 64)`).
// go-dep: github.com/tikv/client-go/v2@v2.0.8-0.20260803075849-c3b50791b9fb.BytesToString L1
pub fn bytes_to_string(num_bytes: i64) -> String {
    let gb = num_bytes as f64 / BYTE_SIZE_GB as f64;
    if gb > 1.0 {
        return format!("{} GB", format_float(gb, b'g', -1, 64));
    }
    let mb = num_bytes as f64 / BYTE_SIZE_MB as f64;
    if mb > 1.0 {
        return format!("{} MB", format_float(mb, b'g', -1, 64));
    }
    let kb = num_bytes as f64 / BYTE_SIZE_KB as f64;
    if kb > 1.0 {
        return format!("{} KB", format_float(kb, b'g', -1, 64));
    }
    format!("{num_bytes} Bytes")
}
