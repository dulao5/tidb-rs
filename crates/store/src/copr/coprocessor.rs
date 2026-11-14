//! go-file: pkg/store/copr/coprocessor.go
//!
//! The coprocessor client. Part 1 (D07.2 spec §2): the cop task, task building (by
//! partition, region and bucket, with the store-batch builder), small-task concurrency,
//! the paging range arithmetic and the request-field conversions. The iterator, its workers
//! and response handling are part 2.

// Package-level consts keep their Go names (mapping rule M-GONAME).
#![allow(non_upper_case_globals)]

use std::collections::{BTreeMap, HashMap};
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::FutureExt;
use tidb_base::gofmt::Arg;
use tidb_memory::tracker::Tracker;

use tidb_base::errors::Error;
use tidb_base::gostd::math;
use tidb_base::kv::{StoreType, TiDB, TiKV};
use tidb_kv::kv::{IsoLevel, PriorityHigh, PriorityLow, RC, RCCheckTS, ReqTypeDAG, Request, SI};
use tidb_kv::option::ReplicaReadLeader;
use tidb_kv::{Key, KeyRange};
use tikv_client::proto::{coprocessor, kvrpcpb, metapb};
use tokio::sync::mpsc;

use super::key_ranges::KeyRanges;
use super::region_cache::RegionCache;
use super::store::{Store, get_end_point_type};
use super::tikv::{
    Backoffer, ClientHelper, RPCContext, RateLimit, RegionVerID, TiKVRegionCache, TiKVStore,
    Variables, get_request_source, merge_from_scan_detail_v2, merge_from_time_detail,
};
use crate::util::paging;

// Maximum total sleep time(in ms) for kv/cop commands.
pub const copBuildTaskMaxBackoff: isize = 5000;
pub const CopNextMaxBackoff: isize = 20000;
/// 32 is the initial batch size of TiKV
pub const CopSmallTaskRow: isize = 32;
const smallTaskSigma: f64 = 0.5;
const smallConcPerCore: isize = 20;

/// client-go `tikvrpc.CmdType`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CmdType(pub u16);

/// client-go `tikvrpc.CmdCop` (`512 + iota` at the 41st constant of its block: 552).
pub const CmdCop: CmdType = CmdType(552);

/// copResponse: one response of a cop task, or the error that ended it.
///
/// `finCopResp` (the "a task finished" marker of the unordered channel) is
/// [`RespMsg::Fin`], not a response value.
#[derive(Debug, Default)]
pub struct CopResponse {
    pub pb_resp: Option<coprocessor::Response>,
    pub detail: Option<Box<CopRuntimeStats>>,
    pub start_key: Key,
    pub err: Option<Error>,
    pub resp_size: i64,
    pub resp_time: Duration,
}

/// A task's response channel, Go's `chan *copResponse` (M-CH): the worker sends on `tx`,
/// the iterator takes `rx`.
#[derive(Debug)]
pub struct RespChan {
    pub tx: mpsc::Sender<Box<CopResponse>>,
    pub rx: Option<mpsc::Receiver<Box<CopResponse>>>,
}

impl RespChan {
    /// `make(chan *copResponse, size)`.
    // glue: make(chan *copResponse, size) (M-CH)
    pub fn new(size: usize) -> RespChan {
        let (tx, rx) = mpsc::channel(size);
        RespChan { tx, rx: Some(rx) }
    }
}

/// copTask contains a related Region and KeyRange for a kv.Request.
///
/// `eventCb` (`trxevents`, the lock-event callback of the worker's lock handling) comes with
/// part 2.
#[derive(Debug, Default)]
pub struct CopTask {
    pub task_id: u64,
    pub region: RegionVerID,
    pub buckets_ver: u64,
    pub ranges: KeyRanges,

    pub resp_chan: Option<RespChan>,
    pub store_addr: String,
    pub cmd_type: CmdType,
    pub store_type: StoreType,

    pub paging: bool,
    pub paging_size: u64,
    pub paging_task_idx: u32,

    /// used by balanceBatchCopTask in PartitionTableScan
    pub partition_index: i64,
    pub request_source: tidb_kv::context::RequestSource,
    /// used for extra concurrency of small tasks, -1 for unknown row count
    pub row_count_hint: isize,
    /// Go's `map[uint64]*batchedCopTask`, nil until the first batched task; keyed by task
    /// id, so it is iterated in key order (M-MAPORDER, plan §3).
    pub batch_task_list: Option<BTreeMap<u64, BatchedCopTask>>,

    /// when this task is batched and the leader's wait duration exceeds the load-based
    /// threshold, we set this field to the target replica store ID and redirect the request
    /// to the replica.
    pub redirect2_replica: Option<u64>,
    pub busy_threshold: Duration,
    pub meet_lock_fallback: bool,

    /// timeout value for one kv readonly request
    pub tikv_client_read_timeout: u64,
    /// firstReadType is used to indicate the type of first read when retrying.
    pub first_read_type: String,
}

/// batchedCopTask: a small cop task carried by another task to the same store.
#[derive(Debug)]
pub struct BatchedCopTask {
    pub task: CopTask,
    pub region: coprocessor::RegionInfo,
    pub store_id: u64,
    pub peer: metapb::Peer,
    pub load_based_replica_retry: bool,
}

/// `len(task.batchTaskList)` (0 for a nil map).
// glue: Go len() of a nil-able map
fn batch_len(task: &CopTask) -> usize {
    task.batch_task_list.as_ref().map_or(0, |m| m.len())
}

impl CopTask {
    // go: copTask.String L1
    pub fn string(&self) -> String {
        format!(
            "region({} {} {}) ranges({}) store({})",
            self.region.get_id(),
            self.region.get_conf_ver(),
            self.region.get_ver(),
            self.ranges.len(),
            self.store_addr
        )
    }

    // go: copTask.ToPBBatchTasks L1 [M-MAPORDER]
    pub fn to_pb_batch_tasks(&self) -> Vec<coprocessor::StoreBatchTask> {
        if batch_len(self) == 0 {
            return Vec::new();
        }
        let list = self.batch_task_list.as_ref().unwrap();
        let mut pb_tasks = Vec::with_capacity(list.len());
        for task in list.values() {
            let store_batch_task = coprocessor::StoreBatchTask {
                region_id: task.region.region_id,
                region_epoch: task.region.region_epoch.clone(),
                peer: Some(task.peer.clone()),
                ranges: task.region.ranges.clone(),
                task_id: task.task.task_id,
                ..Default::default()
            };
            pb_tasks.push(store_batch_task);
        }
        pb_tasks
    }
}

/// rangesPerTask limits the length of the ranges slice sent in one copTask.
pub const rangesPerTask: usize = 25000;

/// buildCopTaskOpt.
///
/// `eventCb` comes with part 2 (see [`CopTask`]); `rangesPerTaskLimit` is Go's
/// `rangesPerTask` overridden by the test-only failpoint `setRangesPerTask`, a field here
/// (plan §3).
pub struct BuildCopTaskOpt<'a, C: TiKVRegionCache> {
    pub req: &'a Request,
    pub cache: &'a RegionCache<C>,
    pub resp_chan: bool,
    pub row_hints: Option<&'a [isize]>,
    pub elapsed: Option<&'a mut Duration>,
    /// ignoreTiKVClientReadTimeout is used to ignore tikv_client_read_timeout
    /// configuration, use default timeout instead.
    pub ignore_tikv_client_read_timeout: bool,
    /// failpoint `setRangesPerTask` (tests only); `rangesPerTask` otherwise.
    pub ranges_per_task_limit: usize,
}

impl<'a, C: TiKVRegionCache> BuildCopTaskOpt<'a, C> {
    /// `&buildCopTaskOpt{req: req, cache: cache}` with Go's zero values for the rest.
    // glue: Go composite literal with zero values (and the failpoint default rangesPerTask)
    pub fn new(req: &'a Request, cache: &'a RegionCache<C>) -> Self {
        BuildCopTaskOpt {
            req,
            cache,
            resp_chan: false,
            row_hints: None,
            elapsed: None,
            ignore_tikv_client_read_timeout: false,
            ranges_per_task_limit: rangesPerTask,
        }
    }
}

// go: buildCopTasks L1 [M-ERR, M-INT]
pub async fn build_cop_tasks<C: TiKVRegionCache>(
    bo: &mut C::Backoffer,
    ranges: &KeyRanges,
    opt: &mut BuildCopTaskOpt<'_, C>,
) -> Result<Vec<CopTask>, Error> {
    let (req, cache) = (opt.req, opt.cache);
    let mut hints = opt.row_hints;
    let start = Instant::now();
    let cmd_type = CmdCop;
    if req.store_type == TiDB {
        return build_tidb_mem_cop_tasks(ranges, req);
    }
    let ranges_len = ranges.len();
    // something went wrong, disable hints to avoid out of range index.
    if hints.map_or(0, |h| h.len()) != ranges_len {
        hints = None;
    }

    let ranges_per_task_limit = opt.ranges_per_task_limit;

    // TODO(youjiali1995): is there any request type that needn't be split by buckets?
    let locs = cache
        .split_key_ranges_by_buckets(bo, ranges.clone())
        .await?;
    // Channel buffer is 2 for handling region split.
    // In a common case, two region split tasks will not be blocked.
    let mut chan_size = 2;
    // in paging request, a request will be returned in multi batches,
    // enlarge the channel size to avoid the request blocked by buffer full.
    if req.paging.enable {
        chan_size = 18;
    }

    let mut builder = if req.store_batch_size > 0 && hints.is_some() {
        TaskBuilder::Batch(new_batch_task_builder(req, cache, req.replica_read))
    } else {
        TaskBuilder::Legacy(new_legacy_task_builder(locs.len()))
    };
    let mut orig_range_idx = 0usize;
    for loc in &locs {
        // TiKV will return gRPC error if the message is too large. So we need to limit the
        // length of the ranges slice to make sure the message can be sent successfully.
        let r_len = loc.ranges.len();
        // If this is a paging request, we set the paging size to minPagingSize,
        // the size will grow every round.
        let mut paging_size = 0u64;
        if req.paging.enable {
            paging_size = req.paging.min_paging_size;
        }
        let mut i = 0;
        while i < r_len {
            let next_i = (i + ranges_per_task_limit).min(r_len);
            let mut hint: isize = -1;
            // calculate the row count hint
            if let Some(hints) = hints {
                let start_key = &loc.ranges.ref_at(i).start_key;
                let end_key = &loc.ranges.ref_at(next_i - 1).end_key;
                // move to the previous range if startKey of current range is lower than
                // endKey of previous location.
                // In the following example, task1 will move origRangeIdx to region(i, z).
                // When counting the row hint for task2, we need to move origRangeIdx back
                // to region(a, h).
                // |<-      region(a, h)    ->| |<-   region(i, z)   ->|
                // |<- task1 ->| |<- task2 ->| ...
                if orig_range_idx > 0
                    && ranges.ref_at(orig_range_idx - 1).end_key.cmp(start_key) > 0
                {
                    orig_range_idx -= 1;
                }
                hint = 0;
                let mut next_orig_range_idx = orig_range_idx;
                while next_orig_range_idx < ranges.len() {
                    let range_start = &ranges.ref_at(next_orig_range_idx).start_key;
                    if range_start.cmp(end_key) > 0 {
                        orig_range_idx = next_orig_range_idx;
                        break;
                    }
                    hint += hints[next_orig_range_idx];
                    next_orig_range_idx += 1;
                }
            }
            let mut task = CopTask {
                region: loc.location.region,
                buckets_ver: loc.get_bucket_version(),
                ranges: loc.ranges.slice(i, next_i),
                cmd_type,
                store_type: req.store_type,
                paging: req.paging.enable,
                paging_size,
                request_source: req.request_source.clone(),
                row_count_hint: hint,
                busy_threshold: req.store_busy_threshold,
                ..Default::default()
            };
            if !opt.ignore_tikv_client_read_timeout {
                task.tikv_client_read_timeout = req.tikv_client_read_timeout;
            }
            // only keep-order need chan inside task.
            // tasks by region error will reuse the channel of parent task.
            if req.keep_order && opt.resp_chan {
                task.resp_chan = Some(RespChan::new(chan_size));
            }
            // Go keeps the task pointer and changes it after `handle`; the builder owns the
            // task here, so it is changed through the builder (L2, plan §3).
            let handled = builder.handle(bo, task).await?;
            i = next_i;
            if req.paging.enable {
                if req.limit_size != 0 && req.limit_size < paging_size {
                    // disable paging for small limit.
                    builder.with_task(handled, |task| {
                        task.paging = false;
                        task.paging_size = 0;
                    });
                } else {
                    paging_size = paging::grow_paging_size(paging_size, req.paging.max_paging_size);
                }
            }
        }
    }

    if req.desc {
        builder.reverse();
    }
    let region_num = builder.region_num();
    let tasks = builder.build();
    let elapsed = start.elapsed();
    if elapsed > Duration::from_millis(500) {
        log::warn!(
            "buildCopTasks takes too much time, elapsed: {elapsed:?}, range len: {ranges_len}, task len: {}",
            tasks.len()
        );
    }
    if let Some(e) = opt.elapsed.as_deref_mut() {
        *e += elapsed;
    }
    // N9: client-go's `TxnRegionsNumHistogramWithCoprocessor` has no client-rust
    // counterpart yet; the observation is dropped (spec §4).
    let _ = region_num;
    Ok(tasks)
}

/// Where `taskBuilder.handle` put the task: Go keeps a pointer to it.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Handled {
    /// `b.tasks[i]`.
    Task(usize),
    /// `b.tasks[i].batchTaskList[id].task`.
    Batched(usize, u64),
}

/// taskBuilder: `legacyTaskBuilder` or `batchStoreTaskBuilder` (M-IFACE: the two
/// implementations of the interface).
pub(crate) enum TaskBuilder<'a, C: TiKVRegionCache> {
    Legacy(LegacyTaskBuilder),
    Batch(BatchStoreTaskBuilder<'a, C>),
}

impl<C: TiKVRegionCache> TaskBuilder<'_, C> {
    // glue: dispatch of the taskBuilder interface method handle (M-IFACE)
    async fn handle(&mut self, bo: &mut C::Backoffer, task: CopTask) -> Result<Handled, Error> {
        match self {
            TaskBuilder::Legacy(b) => Ok(b.handle(task)),
            TaskBuilder::Batch(b) => b.handle(bo, task).await,
        }
    }

    // glue: dispatch of the taskBuilder interface method reverse (M-IFACE)
    fn reverse(&mut self) {
        match self {
            TaskBuilder::Legacy(b) => b.reverse(),
            TaskBuilder::Batch(b) => b.reverse(),
        }
    }

    // glue: dispatch of the taskBuilder interface method build (M-IFACE)
    fn build(self) -> Vec<CopTask> {
        match self {
            TaskBuilder::Legacy(b) => b.build(),
            TaskBuilder::Batch(b) => b.build(),
        }
    }

    // glue: dispatch of the taskBuilder interface method regionNum (M-IFACE)
    fn region_num(&self) -> isize {
        match self {
            TaskBuilder::Legacy(b) => b.region_num(),
            TaskBuilder::Batch(b) => b.region_num(),
        }
    }

    /// The task `handle` placed at `at` (Go: the `*copTask` the caller still holds).
    // glue: Go mutates the handled task through its pointer (L2, plan §3)
    fn with_task(&mut self, at: Handled, f: impl FnOnce(&mut CopTask)) {
        let tasks = match self {
            TaskBuilder::Legacy(b) => &mut b.tasks,
            TaskBuilder::Batch(b) => &mut b.tasks,
        };
        match at {
            Handled::Task(i) => f(&mut tasks[i]),
            Handled::Batched(i, id) => f(&mut tasks[i]
                .batch_task_list
                .as_mut()
                .unwrap()
                .get_mut(&id)
                .unwrap()
                .task),
        }
    }
}

pub(crate) struct LegacyTaskBuilder {
    tasks: Vec<CopTask>,
}

// go: newLegacyTaskBuilder L1
pub(crate) fn new_legacy_task_builder(hint: usize) -> LegacyTaskBuilder {
    LegacyTaskBuilder {
        tasks: Vec::with_capacity(hint),
    }
}

impl LegacyTaskBuilder {
    // go: legacyTaskBuilder.handle L2 (takes the task by value and returns where it went; Go keeps the pointer)
    fn handle(&mut self, task: CopTask) -> Handled {
        self.tasks.push(task);
        Handled::Task(self.tasks.len() - 1)
    }

    // go: legacyTaskBuilder.regionNum L1
    fn region_num(&self) -> isize {
        self.tasks.len() as isize
    }

    // go: legacyTaskBuilder.reverse L1
    fn reverse(&mut self) {
        reverse_tasks(&mut self.tasks);
    }

    // go: legacyTaskBuilder.build L1
    fn build(self) -> Vec<CopTask> {
        self.tasks
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct StoreReplicaKey {
    store_id: u64,
    replica_read: bool,
}

pub(crate) struct BatchStoreTaskBuilder<'a, C: TiKVRegionCache> {
    req: &'a Request,
    cache: &'a RegionCache<C>,
    task_id: u64,
    limit: isize,
    store2_idx: std::collections::HashMap<StoreReplicaKey, usize>,
    tasks: Vec<CopTask>,
    replica_read: tidb_kv::option::ReplicaReadType,
}

/// Go's builder also keeps the `*Backoffer` it was created with; here `handle` takes it,
/// because the caller owns it mutably (plan §3).
// go: newBatchTaskBuilder L1
pub(crate) fn new_batch_task_builder<'a, C: TiKVRegionCache>(
    req: &'a Request,
    cache: &'a RegionCache<C>,
    replica_read: tidb_kv::option::ReplicaReadType,
) -> BatchStoreTaskBuilder<'a, C> {
    BatchStoreTaskBuilder {
        req,
        cache,
        task_id: 0,
        limit: req.store_batch_size,
        store2_idx: std::collections::HashMap::with_capacity(16),
        tasks: Vec::with_capacity(16),
        replica_read,
    }
}

impl<C: TiKVRegionCache> BatchStoreTaskBuilder<'_, C> {
    // go: batchStoreTaskBuilder.handle L2 (takes the task by value and returns where it went; the deferred fallback is the explicit early returns)
    async fn handle(&mut self, bo: &mut C::Backoffer, mut task: CopTask) -> Result<Handled, Error> {
        self.task_id += 1;
        task.task_id = self.task_id;
        // only batch small tasks for memory control.
        if self.limit <= 0 || !is_small_task(&task) {
            // fallback to non-batch way. It's mainly caused by region miss.
            self.tasks.push(task);
            return Ok(Handled::Task(self.tasks.len() - 1));
        }
        let mut batched_task = match self
            .cache
            .build_batch_task(bo, self.req, task, self.replica_read)
            .await?
        {
            Ok(b) => b,
            Err(task) => {
                // fallback to non-batch way. It's mainly caused by region miss.
                self.tasks.push(task);
                return Ok(Handled::Task(self.tasks.len() - 1));
            }
        };
        let key = StoreReplicaKey {
            store_id: batched_task.store_id,
            replica_read: batched_task.load_based_replica_retry,
        };
        let idx = self.store2_idx.get(&key).copied();
        if idx.is_none_or(|idx| batch_len(&self.tasks[idx]) as isize >= self.limit) {
            if batched_task.load_based_replica_retry {
                // If the task is dispatched to leader because all followers are busy,
                // task.redirect2Replica != nil means the busy threshold shouldn't take
                // effect again.
                batched_task.task.redirect2_replica = Some(batched_task.store_id);
            }
            self.tasks.push(batched_task.task);
            self.store2_idx.insert(key, self.tasks.len() - 1);
            Ok(Handled::Task(self.tasks.len() - 1))
        } else {
            let idx = idx.unwrap();
            let carrier = &mut self.tasks[idx];
            if carrier.batch_task_list.is_none() {
                carrier.batch_task_list = Some(BTreeMap::new());
                // disable paging for batched task.
                carrier.paging = false;
                carrier.paging_size = 0;
                // The task and it's batched can be served only in the store we chose.
                // If the task is redirected to other replica, the batched task may not meet
                // region-miss or store-not-match error.
                // So disable busy threshold for the task which carries batched tasks.
                carrier.busy_threshold = Duration::ZERO;
            }
            if batched_task.task.row_count_hint > 0 {
                carrier.row_count_hint += batched_task.task.row_count_hint;
            }
            batched_task.task.paging = false;
            batched_task.task.paging_size = 0;
            batched_task.task.busy_threshold = Duration::ZERO;
            let id = batched_task.task.task_id;
            carrier
                .batch_task_list
                .as_mut()
                .unwrap()
                .insert(id, batched_task);
            Ok(Handled::Batched(idx, id))
        }
    }

    // go: batchStoreTaskBuilder.regionNum L1
    fn region_num(&self) -> isize {
        // we allocate b.taskID for each region task, so the final b.taskID is equal to the
        // related region number.
        self.task_id as isize
    }

    // go: batchStoreTaskBuilder.reverse L1
    fn reverse(&mut self) {
        reverse_tasks(&mut self.tasks);
    }

    // go: batchStoreTaskBuilder.build L1
    fn build(self) -> Vec<CopTask> {
        self.tasks
    }
}

/// Tasks for the coprocessor of the TiDB servers themselves (memory tables of the
/// cluster), one per server.
///
/// Boundary stub: the server list is `infosync.GetAllServerInfo` (D06.5). Until then a
/// request to `kv.TiDB` fails with this error instead of reading nothing.
// glue: boundary stub (blocked-by:D06.5 infosync.GetAllServerInfo; constitution §4.3)
pub(crate) fn build_tidb_mem_cop_tasks(
    _ranges: &KeyRanges,
    _req: &Request,
) -> Result<Vec<CopTask>, Error> {
    Err(Error::plain(
        "coprocessor on TiDB servers needs infosync.GetAllServerInfo, which is not ported yet (D06.5)",
    ))
}

// go: reverseTasks L1
pub(crate) fn reverse_tasks(tasks: &mut [CopTask]) {
    let n = tasks.len();
    for i in 0..n / 2 {
        let j = n - i - 1;
        tasks.swap(i, j);
    }
}

/// strictly, only RowCountHint == -1 stands for unknown task rows,
/// but when RowCountHint == 0, it may be caused by initialized value,
/// to avoid the future bugs, let the tasks with RowCountHint == 0 be non-small tasks.
///
/// Go's `&&` binds tighter than `||`: a task carrying batched tasks is small when its hint
/// is at most `2*CopSmallTaskRow`, zero and negative hints included. Kept as in Go.
// go: isSmallTask L1
pub fn is_small_task(task: &CopTask) -> bool {
    task.row_count_hint > 0 && (batch_len(task) == 0 && task.row_count_hint <= CopSmallTaskRow)
        || (batch_len(task) > 0 && task.row_count_hint <= 2 * CopSmallTaskRow)
}

/// smallTaskConcurrency counts the small tasks of tasks,
/// then returns the task count and extra concurrency for small tasks.
///
/// `1 + smallTaskSigma*math.Sqrt(...)` may be fused into an FMA by gc on arm64; with
/// `smallTaskSigma = 0.5` the product is exact, so fused and unfused results are equal.
// go: smallTaskConcurrency L1 [M-INT, M-F2I]
pub fn small_task_concurrency(tasks: &[CopTask], mut numcpu: isize) -> (isize, isize) {
    let mut res: isize = 0;
    for task in tasks {
        if is_small_task(task) {
            res += 1;
        }
    }
    if res == 0 {
        return (0, 0);
    }
    // Calculate the extra concurrency for small tasks
    // extra concurrency = tasks / (1 + sigma * sqrt(log(tasks ^ 2)))
    let mut extra_conc =
        (res as f64 / (1.0 + smallTaskSigma * (2.0 * math::log(res as f64)).sqrt())) as isize;
    if numcpu <= 0 {
        numcpu = 1;
    }
    let small_task_concurrency_limit = smallConcPerCore * numcpu;
    if extra_conc > small_task_concurrency_limit {
        extra_conc = small_task_concurrency_limit;
    }
    (res, extra_conc)
}

/// calculateRetry splits the input ranges into two, and take one of them according to desc
/// flag. It's used in paging API, to calculate which range is consumed and what needs to be
/// retry.
/// For example:
/// ranges: [r1 --> r2) [r3 --> r4)
/// split:      [s1   -->   s2)
/// In normal scan order, all data before s1 is consumed, so the retry ranges should be
/// [s1 --> r2) [r3 --> r4)
/// In reverse scan order, all data after s2 is consumed, so the retry ranges should be
/// [r1 --> r2) [r3 --> s2)
///
/// A method of `copIteratorWorker` in Go that reads nothing of the worker.
// go: copIteratorWorker.calculateRetry L1
pub fn calculate_retry(
    ranges: &KeyRanges,
    split: Option<&coprocessor::KeyRange>,
    desc: bool,
) -> KeyRanges {
    let Some(split) = split else {
        return ranges.clone();
    };
    if desc {
        let (left, _) = ranges.split(&split.end);
        return left;
    }
    let (_, right) = ranges.split(&split.start);
    right
}

/// calculateRemain calculates the remain ranges to be processed, it's used in paging API.
/// For example:
/// ranges: [r1 --> r2) [r3 --> r4)
/// split:      [s1   -->   s2)
/// In normal scan order, all data before s2 is consumed, so the remained ranges should be
/// [s2 --> r4)
/// In reverse scan order, all data after s1 is consumed, so the remained ranges should be
/// [r1 --> s1)
///
/// A method of `copIteratorWorker` in Go that reads nothing of the worker.
// go: copIteratorWorker.calculateRemain L1
pub fn calculate_remain(
    ranges: &KeyRanges,
    split: Option<&coprocessor::KeyRange>,
    desc: bool,
) -> KeyRanges {
    let Some(split) = split else {
        return ranges.clone();
    };
    if desc {
        let (left, _) = ranges.split(&split.start);
        return left;
    }
    let (_, right) = ranges.split(&split.end);
    right
}

/// priorityToPB converts priority type to wire type.
// go: priorityToPB L1
pub fn priority_to_pb(pri: isize) -> kvrpcpb::CommandPri {
    match pri {
        PriorityLow => kvrpcpb::CommandPri::Low,
        PriorityHigh => kvrpcpb::CommandPri::High,
        _ => kvrpcpb::CommandPri::Normal,
    }
}

// go: isolationLevelToPB L1
pub fn isolation_level_to_pb(level: IsoLevel) -> kvrpcpb::IsolationLevel {
    match level {
        RC => kvrpcpb::IsolationLevel::Rc,
        SI => kvrpcpb::IsolationLevel::Si,
        RCCheckTS => kvrpcpb::IsolationLevel::RcCheckTs,
        _ => kvrpcpb::IsolationLevel::Si,
    }
}

/// BuildKeyRanges is used for test, quickly build key ranges from paired keys.
///
/// Go reads `keys[i+1]` for an odd count and panics; so does the port.
// go: BuildKeyRanges L1
pub fn build_key_ranges(keys: &[&[u8]]) -> Vec<KeyRange> {
    let mut ranges = Vec::new();
    let mut i = 0;
    while i < keys.len() {
        ranges.push(KeyRange {
            start_key: Key(keys[i].to_vec()),
            end_key: Key(keys[i + 1].to_vec()),
        });
        i += 2;
    }
    ranges
}

/// The failpoint `disableFixedRowCountHint` (tests only) is not ported.
// go: optRowHint L1
pub fn opt_row_hint(req: &Request) -> bool {
    let opt = true;
    if req.store_type == TiDB {
        return false;
    }
    if req.request_source.request_source_internal || req.tp != ReqTypeDAG {
        // disable extra concurrency for internal tasks.
        return false;
    }
    opt
}

// go: checkStoreBatchCopr L1
pub fn check_store_batch_copr(req: &Request) -> bool {
    if req.tp != ReqTypeDAG || req.store_type != TiKV {
        return false;
    }
    // TODO: support keep-order batch
    if req.replica_read != ReplicaReadLeader || req.keep_order {
        // Disable batch copr for follower read
        return false;
    }
    // Disable batch copr when paging is enabled.
    if req.paging.enable {
        return false;
    }
    // Disable it for internal requests to avoid regression.
    if req.request_source.request_source_internal {
        return false;
    }
    true
}

// ---------------------------------------------------------------- part 2: client, iterator, workers

/// The marker Go sends as `finCopResp` on the unordered channel, or a response.
#[derive(Debug)]
pub enum RespMsg {
    Resp(Box<CopResponse>),
    /// `finCopResp`: one task finished.
    Fin,
}

/// CopRuntimeStats contains execution detail information.
///
/// `ReqStats` (client-go `RegionRequestRuntimeStats`) has no client-rust counterpart and is
/// never set (D07.2 spec §6).
#[derive(Clone, Debug, Default)]
pub struct CopRuntimeStats {
    pub cop_exec_details: tidb_execdetails::execdetails::CopExecDetails,
    pub copr_cache_hit: bool,
}

/// `unsafe.Sizeof(execdetails.ExecDetails{})` on 64-bit Go (v8.5.8).
const sizeofExecDetails: i64 = 384;

impl CopResponse {
    /// GetData implements the kv.ResultSubset GetData interface.
    // go: copResponse.GetData L1
    pub fn get_data(&self) -> &[u8] {
        self.pb_resp.as_ref().map_or(&[], |r| r.data.as_slice())
    }

    /// GetStartKey implements the kv.ResultSubset GetStartKey interface.
    // go: copResponse.GetStartKey L1
    pub fn get_start_key(&self) -> &Key {
        &self.start_key
    }

    // go: copResponse.GetCopRuntimeStats L1
    pub fn get_cop_runtime_stats(&self) -> Option<&CopRuntimeStats> {
        self.detail.as_deref()
    }

    /// MemSize returns how many bytes of memory this response use.
    ///
    /// Go caches the result in `respSize`; the value only depends on the response, which is
    /// not changed afterwards, so the port computes it again (L2). `pbResp.Size()` is the
    /// encoded size, prost's `encoded_len`.
    // go: copResponse.MemSize L2 (recomputed instead of cached in respSize; the finCopResp case is RespMsg::Fin)
    pub fn mem_size(&self) -> i64 {
        if self.resp_size != 0 {
            return self.resp_size;
        }
        let mut size = self.start_key.0.capacity() as i64;
        if self.detail.is_some() {
            size += sizeofExecDetails;
        }
        if let Some(r) = &self.pb_resp {
            size += prost::Message::encoded_len(r) as i64;
        }
        size
    }

    // go: copResponse.RespTime L1
    pub fn resp_time(&self) -> Duration {
        self.resp_time
    }
}

impl tidb_kv::kv::ResultSubset for CopResponse {
    // glue: kv.ResultSubset implementation (GetData)
    fn get_data(&self) -> &[u8] {
        CopResponse::get_data(self)
    }
    // glue: kv.ResultSubset implementation (GetStartKey)
    fn get_start_key(&self) -> &Key {
        CopResponse::get_start_key(self)
    }
    // glue: kv.ResultSubset implementation (MemSize)
    fn mem_size(&self) -> i64 {
        CopResponse::mem_size(self)
    }
    // glue: kv.ResultSubset implementation (RespTime)
    fn resp_time(&self) -> Duration {
        CopResponse::resp_time(self)
    }
}

const minLogCopTaskTime: Duration = Duration::from_millis(300);
const minLogBackoffTime: i64 = 100;
const minLogKVProcessTime: u64 = 100;

/// MockResponseSizeForTest mock the response size
pub const MockResponseSizeForTest: i64 = 100 * 1024 * 1024;

/// CopClient is coprocessor client.
pub struct CopClient<S: TiKVStore> {
    pub(crate) store: Arc<Store<S>>,
    pub(crate) replica_read_seed: u32,
}

impl<S: TiKVStore> CopClient<S> {
    /// Send builds the request and gets the coprocessor iterator response.
    ///
    /// Building the tasks locates regions (I/O), which `kv.Client.Send` (a synchronous
    /// interface) cannot await: the build runs at the first `Next`, where Go's
    /// `copErrorResponse` would report a build error too (L2, spec §6). TiFlash batch cop is
    /// DEF.MPP and fails.
    // go: CopClient.Send L2 (the build runs at the first Next; context values are the N9 stubs)
    pub fn send(
        &self,
        req: &Request,
        variables: Option<&tidb_kv::kv::OptionValue>,
        option: &tidb_kv::kv::ClientSendOption,
    ) -> Box<dyn tidb_kv::kv::Response> {
        let Some(vars) = variables.and_then(|v| v.downcast_ref::<Variables>()) else {
            return Box::new(CopErrorResponse(Some(tidb_base::errors::errorf(
                "unsupported variables:%+v",
                &[Arg::Str(b"<nil>".to_vec().into())],
            ))));
        };
        if req.store_type == tidb_base::kv::TiFlash && req.batch_cop {
            return Box::new(CopErrorResponse(Some(Error::plain(
                "batch coprocessor requests to TiFlash are not supported (DEF.MPP)",
            ))));
        }
        Box::new(LazyCopResponse {
            pending: Some((req.clone(), vars.clone(), option.clone())),
            store: self.store.clone(),
            replica_read_seed: self.replica_read_seed,
            it: None,
            err: None,
        })
    }

    /// BuildCopIterator builds the iterator without calling `open`.
    // go: CopClient.BuildCopIterator L1 [M-ERR, M-MEM]
    #[allow(clippy::collapsible_if)] // Go's nested ifs
    pub async fn build_cop_iterator(
        &self,
        mut req: Request,
        vars: Variables,
        option: &tidb_kv::kv::ClientSendOption,
    ) -> Result<CopIterator<S>, Error> {
        if req.store_type == TiDB {
            // coprocessor on TiDB doesn't support paging
            req.paging.enable = false;
        }
        if req.tp != ReqTypeDAG {
            // coprocessor request but type is not DAG
            req.paging.enable = false;
        }
        if !check_store_batch_copr(&req) {
            req.store_batch_size = 0;
        }

        let mut bo = self
            .store
            .kv_store
            .new_backoffer(copBuildTaskMaxBackoff as i64, &vars);
        let mut tasks: Vec<CopTask> = Vec::new();
        let try_row_hint = opt_row_hint(&req);
        let mut elapsed = Duration::ZERO;
        // Here we build the task by partition, not directly by region.
        // This is because it's possible that TiDB merge multiple small partition into one
        // region which break some assumption.
        // Keep it split by partition would be more safe.
        // (Go passes a closure to ForEachPartitionWithErr; the closure awaits here, so the
        // partitions are collected first, L2.)
        let mut parts: Vec<(Vec<KeyRange>, Option<Vec<isize>>)> = Vec::new();
        if let Some(krs) = &req.key_ranges {
            krs.for_each_partition_with_err(|r, h| {
                parts.push((r.to_vec(), h.map(|h| h.to_vec())));
                Ok(())
            })?;
        }
        let cache = self.store.get_region_cache();
        let mut result: Result<(), Error> = Ok(());
        for (ranges, hints) in &parts {
            let key_ranges = super::key_ranges::new_key_ranges(ranges.clone());
            let mut opt = BuildCopTaskOpt::new(&req, &cache);
            opt.resp_chan = req.keep_order;
            opt.elapsed = Some(&mut elapsed);
            if try_row_hint {
                opt.row_hints = hints.as_deref();
            }
            match build_cop_tasks(&mut bo, &key_ranges, &mut opt).await {
                Ok(t) => tasks.extend(t),
                Err(e) => {
                    result = Err(e);
                    break;
                }
            }
        }
        // only batch store requests in first build.
        req.store_batch_size = 0;
        if let Some(adjuster) = req.closest_replica_read_adjuster.clone() {
            let n = tasks.len() as isize;
            adjuster(&mut req, n);
        }
        // N9: DistSQLCoprClosestReadCounter is not recorded (spec §6).
        result?;
        let (finish_tx, finish_rx) = tokio::sync::watch::channel(false);
        let mut concurrency = req.concurrency;
        if concurrency > tasks.len() as isize {
            concurrency = tasks.len() as isize;
        }
        let mut small_task_concurrency = 0;
        if try_row_hint {
            let small_tasks;
            (small_tasks, small_task_concurrency) =
                small_task_concurrency_fn(&tasks, self.store.numcpu);
            if tasks.len() as isize - small_tasks < concurrency {
                concurrency = tasks.len() as isize - small_tasks;
            }
        }
        if concurrency < 1 {
            // Make sure that there is at least one worker.
            concurrency = 1;
        }
        // N10: no runaway checker before resource control (the CoolDown branch).
        let keep_order = req.keep_order;
        let send_rate = if keep_order {
            if small_task_concurrency > 20 {
                small_task_concurrency = 20;
            }
            RateLimit::new((2 * (concurrency + small_task_concurrency)) as usize)
        } else {
            RateLimit::new((concurrency + small_task_concurrency) as usize)
        };
        let stats = option
            .enable_collect_execution_info
            .then(|| Arc::new(Mutex::new(Vec::new())));
        let action_on_exceed = Arc::new(new_rate_limit_action(send_rate.get_capacity()));
        if let Some(session) = &option.session_mem_tracker {
            if option.enabled_rate_limit_action {
                session.fallback_old_and_set_new_action(Some(action_on_exceed.clone()));
            }
        }
        action_on_exceed.set_enabled(option.enabled_rate_limit_action);
        let start_ts = req.start_ts;
        let resolved = !req.is_staleness;
        let mem_tracker = req.mem_tracker.clone();
        let shared = Arc::new(CopIteratorShared {
            store: self.store.clone(),
            kvclient: self.store.kv_store.new_client_helper(start_ts, resolved),
            req: Arc::new(req),
            vars,
            mem_tracker,
            paging_task_idx: AtomicU32::new(0),
            store_batched_num: AtomicU64::new(0),
            store_batched_fallback_num: AtomicU64::new(0),
            stats,
            finish: finish_rx,
        });
        Ok(CopIterator {
            shared,
            concurrency,
            small_task_concurrency,
            lite_worker: None,
            finish_tx,
            closed: AtomicBool::new(false),
            tasks,
            task_rx: Vec::new(),
            curr: 0,
            send_rate: Arc::new(send_rate),
            resp_chan: None,
            keep_order,
            handles: Vec::new(),
            action_on_exceed,
            build_task_elapsed: elapsed,
        })
    }
}

/// `smallTaskConcurrency` under another name inside `BuildCopIterator` (a local variable
/// there shadows it in Go's spelling).
// glue: alias of small_task_concurrency for use beside a local of the same name
fn small_task_concurrency_fn(tasks: &[CopTask], numcpu: isize) -> (isize, isize) {
    small_task_concurrency(tasks, numcpu)
}

/// What the workers of one iterator share: the fields of Go's `copIterator` that
/// `newCopIteratorWorker` copies into each worker.
pub(crate) struct CopIteratorShared<S: TiKVStore> {
    store: Arc<Store<S>>,
    req: Arc<Request>,
    vars: Variables,
    mem_tracker: Option<Arc<Tracker>>,
    kvclient: S::Helper,
    paging_task_idx: AtomicU32,
    store_batched_num: AtomicU64,
    store_batched_fallback_num: AtomicU64,
    stats: Option<Arc<Mutex<Vec<CopRuntimeStats>>>>,
    finish: tokio::sync::watch::Receiver<bool>,
}

/// copIterator.
pub struct CopIterator<S: TiKVStore> {
    shared: Arc<CopIteratorShared<S>>,
    concurrency: isize,
    small_task_concurrency: isize,
    /// liteWorker uses to send cop request without start new goroutine, it is only work
    /// when tasks count is 1.
    lite_worker: Option<LiteCopIteratorWorker<S>>,
    finish_tx: tokio::sync::watch::Sender<bool>,
    closed: AtomicBool,
    /// If keepOrder, results are stored in copTask.respChan, read them out one by one.
    tasks: Vec<CopTask>,
    /// The receiving ends of the tasks' channels (keep order), in task order.
    task_rx: Vec<Option<mpsc::Receiver<Box<CopResponse>>>>,
    /// curr indicates the curr id of the finished copTask
    curr: usize,
    send_rate: Arc<RateLimit>,
    /// Otherwise, results are stored in respChan.
    resp_chan: Option<mpsc::Receiver<RespMsg>>,
    keep_order: bool,
    /// Go's `wg`: the workers and the task sender (M-WG).
    handles: Vec<tokio::task::JoinHandle<()>>,
    action_on_exceed: Arc<RateLimitAction>,
    build_task_elapsed: Duration,
}

/// liteCopIteratorWorker.
pub(crate) struct LiteCopIteratorWorker<S: TiKVStore> {
    worker: CopIteratorWorker<S>,
    batch_cop_resp_list: std::collections::VecDeque<Box<CopResponse>>,
    try_cop_lite_worker: Arc<AtomicU32>,
}

/// Where a worker sends the responses of one task.
enum RespSink {
    Shared(mpsc::Sender<RespMsg>),
    Task(mpsc::Sender<Box<CopResponse>>),
}

/// copIteratorWorker receives tasks from copIteratorTaskSender, handles tasks and sends the
/// copResponse to respChan.
pub(crate) struct CopIteratorWorker<S: TiKVStore> {
    shared: Arc<CopIteratorShared<S>>,
    task_ch: Option<Arc<tokio::sync::Mutex<mpsc::Receiver<CopTask>>>>,
    resp_chan: Option<mpsc::Sender<RespMsg>>,
}

/// copIteratorTaskSender sends tasks to taskCh then wait for the workers to exit.
struct CopIteratorTaskSender {
    task_ch: mpsc::Sender<CopTask>,
    small_task_ch: Option<mpsc::Sender<CopTask>>,
    tasks: Vec<CopTask>,
    finish: tokio::sync::watch::Receiver<bool>,
    send_rate: Arc<RateLimit>,
}

/// copTaskResult.
#[derive(Default)]
#[allow(clippy::vec_box)] // Go's []*copResponse; the boxes travel through the channels
pub(crate) struct CopTaskResult {
    resp: Option<Box<CopResponse>>,
    batch_resp_list: Vec<Box<CopResponse>>,
    remains: Vec<CopTask>,
}

/// Spawns a task on the store's ExecPool (M-GO), on the current tokio runtime without one
/// (tests).
// glue: go f() on the N3 ExecPool
fn spawn<S: TiKVStore>(
    store: &Store<S>,
    name: &'static str,
    fut: impl std::future::Future<Output = ()> + Send + 'static,
) -> tokio::task::JoinHandle<()> {
    match &store.pool {
        Some(pool) => pool.spawn(name, fut),
        None => tokio::spawn(fut),
    }
}

impl<S: TiKVStore> CopIterator<S> {
    /// open starts workers and sender goroutines.
    // go: copIterator.open L1 [M-GO, M-CH, M-WG]
    #[allow(clippy::collapsible_if)] // Go's nested ifs
    pub fn open(&mut self, try_cop_lite_worker: Option<&Arc<AtomicU32>>) {
        if self.tasks.len() == 1 {
            if let Some(flag) = try_cop_lite_worker {
                if flag
                    .compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
                {
                    // For a query, only one `copIterator` can use `liteWorker`.
                    self.lite_worker = Some(LiteCopIteratorWorker {
                        worker: new_cop_iterator_worker(self, None),
                        batch_cop_resp_list: Default::default(),
                        try_cop_lite_worker: flag.clone(),
                    });
                    return;
                }
            }
        }
        let (task_tx, task_rx) = mpsc::channel(1);
        let task_rx = Arc::new(tokio::sync::Mutex::new(task_rx));
        let (small_tx, small_rx) = if self.small_task_concurrency > 0 {
            let (tx, rx) = mpsc::channel(1);
            (Some(tx), Some(Arc::new(tokio::sync::Mutex::new(rx))))
        } else {
            (None, None)
        };
        // Go `make(chan *copResponse)` is unbuffered; a tokio channel holds at least one
        // message (L2, plan §4).
        let resp_tx = if !self.keep_order {
            let (tx, rx) = mpsc::channel(1);
            self.resp_chan = Some(rx);
            Some(tx)
        } else {
            None
        };
        // The receiving ends stay with the iterator; the tasks (with their senders) go to the
        // workers.
        self.task_rx = self
            .tasks
            .iter_mut()
            .map(|t| t.resp_chan.as_mut().and_then(|c| c.rx.take()))
            .collect();
        // Start it.concurrency number of workers to handle cop requests.
        for i in 0..(self.concurrency + self.small_task_concurrency) {
            let mut ch = task_rx.clone();
            if i >= self.concurrency {
                if let Some(s) = &small_rx {
                    ch = s.clone();
                }
            }
            let mut worker = new_cop_iterator_worker(self, Some(ch));
            worker.resp_chan = resp_tx.clone();
            let h = spawn(&self.shared.store, "copIteratorWorker", async move {
                worker.run().await
            });
            self.handles.push(h);
        }
        drop(resp_tx);
        let sender = CopIteratorTaskSender {
            task_ch: task_tx,
            small_task_ch: small_tx,
            tasks: std::mem::take(&mut self.tasks),
            finish: self.shared.finish.clone(),
            send_rate: self.send_rate.clone(),
        };
        let h = spawn(&self.shared.store, "copIteratorTaskSender", sender.run());
        self.handles.push(h);
        // `it.tasks` keeps one slot per task for `Next` (keep order) and `GetTasks`.
        self.curr = 0;
    }

    /// The `select` of `recvFromRespCh` on a task channel: `(resp, ok, exit)`.
    // go: copIterator.recvFromRespCh L1 [M-CH]
    async fn recv_from_resp_ch(
        &mut self,
        ctx: &tidb_kv::Context,
        idx: Option<usize>,
    ) -> (Option<RespMsg>, bool, bool) {
        let _ = ctx; // cancellation: dropping the future (N3 3.1 point 6)
        let mut finish = self.shared.finish.clone();
        let resp = match idx {
            Some(i) => {
                let Some(rx) = self.task_rx[i].as_mut() else {
                    return (None, false, false);
                };
                tokio::select! {
                    r = rx.recv() => r.map(RespMsg::Resp),
                    _ = finish.wait_for(|d| *d) => return (None, false, true),
                }
            }
            None => {
                let rx = self
                    .resp_chan
                    .as_mut()
                    .expect("unordered iterator has respChan");
                tokio::select! {
                    r = rx.recv() => r,
                    _ = finish.wait_for(|d| *d) => return (None, false, true),
                }
            }
        };
        let ok = resp.is_some();
        if let Some(RespMsg::Resp(r)) = &resp {
            mem_tracker_consume_resp(self.shared.mem_tracker.as_ref(), r);
        }
        (resp, ok, false)
    }

    /// GetConcurrency returns the concurrency and small task concurrency.
    // go: copIterator.GetConcurrency L1
    pub fn get_concurrency(&self) -> (isize, isize) {
        (self.concurrency, self.small_task_concurrency)
    }

    /// GetStoreBatchInfo returns the batched and fallback num.
    // go: copIterator.GetStoreBatchInfo L1
    pub fn get_store_batch_info(&self) -> (u64, u64) {
        (
            self.shared.store_batched_num.load(Ordering::SeqCst),
            self.shared
                .store_batched_fallback_num
                .load(Ordering::SeqCst),
        )
    }

    /// GetBuildTaskElapsed returns the duration of building task.
    // go: copIterator.GetBuildTaskElapsed L1
    pub fn get_build_task_elapsed(&self) -> Duration {
        self.build_task_elapsed
    }

    /// GetSendRate returns the rate-limit object.
    // go: copIterator.GetSendRate L1
    pub fn get_send_rate(&self) -> &Arc<RateLimit> {
        &self.send_rate
    }

    /// GetRequestRateLimit returns the shared request rate-limit object.
    ///
    /// `kv.Request.CoprRequestRateLimit` is not ported yet (D07.0 spec §4 point 3): none.
    // go: copIterator.GetRequestRateLimit L1
    pub fn get_request_rate_limit(&self) -> Option<&Arc<RateLimit>> {
        None
    }

    /// GetTasks returns the built tasks (before `open`).
    // go: copIterator.GetTasks L1
    pub fn get_tasks(&self) -> &[CopTask] {
        &self.tasks
    }

    /// Next returns next coprocessor result.
    /// NOTE: Use nil to indicate finish, so if the returned ResultSubset is not nil, reader
    /// should continue to call Next().
    // go: copIterator.Next L1 [M-CH, M-ERR]
    pub async fn next(
        &mut self,
        ctx: &tidb_kv::Context,
    ) -> Result<Option<Box<CopResponse>>, Error> {
        let mut resp: Box<CopResponse>;
        if self.lite_worker.is_some() {
            match self.lite_send_req().await {
                None => {
                    self.action_on_exceed.close();
                    return Ok(None);
                }
                Some(r) => {
                    self.action_on_exceed.destroy_token_if_needed(|| {});
                    mem_tracker_consume_resp(self.shared.mem_tracker.as_ref(), &r);
                    resp = r;
                }
            }
        } else if self.resp_chan.is_some() {
            loop {
                // Get next fetched resp from chan
                let (r, ok, closed) = self.recv_from_resp_ch(ctx, None).await;
                if !ok || closed {
                    self.action_on_exceed.close();
                    return Ok(None);
                }
                match r {
                    Some(RespMsg::Fin) => {
                        let send_rate = self.send_rate.clone();
                        self.action_on_exceed
                            .destroy_token_if_needed(|| send_rate.put_token());
                        continue;
                    }
                    Some(RespMsg::Resp(r)) => {
                        resp = r;
                        break;
                    }
                    None => unreachable!(),
                }
            }
        } else {
            loop {
                if self.curr >= self.task_rx.len() {
                    // Resp will be nil if iterator is finishCh.
                    self.action_on_exceed.close();
                    return Ok(None);
                }
                let (r, ok, closed) = self.recv_from_resp_ch(ctx, Some(self.curr)).await;
                if closed {
                    // Close() is called or context cancelled/timeout, so Next() is invalid.
                    return Ok(None);
                }
                if ok {
                    let Some(RespMsg::Resp(r)) = r else {
                        unreachable!()
                    };
                    resp = r;
                    break;
                }
                let send_rate = self.send_rate.clone();
                self.action_on_exceed
                    .destroy_token_if_needed(|| send_rate.put_token());
                // Switch to next task.
                self.task_rx[self.curr] = None;
                self.curr += 1;
            }
        }

        if let Some(err) = resp.err.take() {
            return Err(err);
        }

        self.shared
            .store
            .check_visibility(self.shared.req.start_ts)?;
        Ok(Some(resp))
    }

    // go: liteCopIteratorWorker.liteSendReq L1 [M-PANIC]
    #[allow(clippy::collapsible_if)] // Go's nested ifs
    async fn lite_send_req(&mut self) -> Option<Box<CopResponse>> {
        let lite = self.lite_worker.as_mut().unwrap();
        if let Some(r) = lite.batch_cop_resp_list.pop_front() {
            return Some(r);
        }
        let mut backoffermap: HashMap<u64, S::Backoffer> = HashMap::new();
        while !self.tasks.is_empty() {
            let cur_task = self.tasks.remove(0);
            let lite = self.lite_worker.as_mut().unwrap();
            let bo = choose_backoffer(&mut backoffermap, &cur_task, &lite.worker);
            let result = AssertUnwindSafe(lite.worker.handle_task_once(bo, cur_task))
                .catch_unwind()
                .await;
            let result = match result {
                Ok(r) => r,
                Err(p) => {
                    log::error!("copIteratorWork meet panic");
                    return Some(Box::new(CopResponse {
                        err: Some(get_recover_error(p)),
                        ..Default::default()
                    }));
                }
            };
            let result = match result {
                Ok(r) => r,
                Err(e) => {
                    let mut resp = Box::new(CopResponse {
                        err: Some(e),
                        ..Default::default()
                    });
                    lite.worker.check_resp_oom(&mut resp);
                    return Some(resp);
                }
            };
            let lite = self.lite_worker.as_mut().unwrap();
            let mut result = result;
            if let Some(r) = result.as_mut() {
                if !r.remains.is_empty() {
                    let mut remains = std::mem::take(&mut r.remains);
                    remains.append(&mut self.tasks);
                    self.tasks = remains;
                }
            }
            if self.tasks.is_empty() {
                // if all tasks are finished, reset tryCopLiteWorker to 0 to make future
                // request can reuse copLiteWorker.
                lite.try_cop_lite_worker.store(0, Ordering::SeqCst);
            }
            if let Some(r) = result {
                if let Some(resp) = r.resp {
                    lite.batch_cop_resp_list = r.batch_resp_list.into();
                    return Some(resp);
                }
                if !r.batch_resp_list.is_empty() {
                    let mut list: std::collections::VecDeque<_> = r.batch_resp_list.into();
                    let resp = list.pop_front();
                    lite.batch_cop_resp_list = list;
                    return resp;
                }
            }
        }
        None
    }

    // go: copIterator.CollectUnconsumedCopRuntimeStats L1 [M-LOCK]
    pub fn collect_unconsumed_cop_runtime_stats(&self) -> Vec<CopRuntimeStats> {
        match &self.shared.stats {
            None => Vec::new(),
            Some(s) => s.lock().unwrap().clone(),
        }
    }

    // go: copIterator.Close L1 [M-WG]
    pub async fn close(&mut self) -> Result<(), Error> {
        if self
            .closed
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            let _ = self.finish_tx.send(true);
        }
        // `rpcCancel.CancelAll()`: the workers stop at their next send or finish check; an
        // RPC in flight completes (no per-RPC canceller in client-rust, spec §6).
        self.action_on_exceed.close();
        for h in self.handles.drain(..) {
            let _ = h.await;
        }
        Ok(())
    }
}

/// Associate each region with an independent backoffer. In this way, when multiple regions
/// are unavailable, TiDB can execute very quickly without blocking
// go: chooseBackoffer L1
fn choose_backoffer<'m, S: TiKVStore>(
    backoffermap: &'m mut HashMap<u64, S::Backoffer>,
    task: &CopTask,
    worker: &CopIteratorWorker<S>,
) -> &'m mut S::Backoffer {
    backoffermap.entry(task.region.get_id()).or_insert_with(|| {
        worker
            .shared
            .store
            .kv_store
            .new_backoffer(CopNextMaxBackoff as i64, &worker.shared.vars)
    })
}

// go: newCopIteratorWorker L1
fn new_cop_iterator_worker<S: TiKVStore>(
    it: &CopIterator<S>,
    task_ch: Option<Arc<tokio::sync::Mutex<mpsc::Receiver<CopTask>>>>,
) -> CopIteratorWorker<S> {
    CopIteratorWorker {
        shared: it.shared.clone(),
        task_ch,
        resp_chan: None,
    }
}

impl CopIteratorTaskSender {
    // go: copIteratorTaskSender.run L1 [M-CH]
    async fn run(self) {
        let CopIteratorTaskSender {
            task_ch,
            small_task_ch,
            tasks,
            mut finish,
            send_rate,
        } = self;
        // Send tasks to feed the worker goroutines.
        for t in tasks {
            // we control the sending rate to prevent all tasks being done (aka. all of the
            // responses are buffered) by copIteratorWorker.
            let exit = send_rate.get_token(&mut finish).await;
            if exit {
                break;
            }
            let send_to = match &small_task_ch {
                Some(small) if is_small_task(&t) => small,
                _ => &task_ch,
            };
            if send_to_task_ch(t, send_to, &mut finish).await {
                break;
            }
        }
        // close(sender.taskCh); the workers' wait group and closing respChan are the
        // iterator's handles and the dropped senders.
        drop(task_ch);
        drop(small_task_ch);
    }
}

// go: copIteratorTaskSender.sendToTaskCh L1 [M-CH]
async fn send_to_task_ch(
    t: CopTask,
    send_to: &mpsc::Sender<CopTask>,
    finish: &mut tokio::sync::watch::Receiver<bool>,
) -> bool {
    tokio::select! {
        r = send_to.send(t) => r.is_err(),
        _ = finish.wait_for(|d| *d) => true,
    }
}

// go: memTrackerConsumeResp L1 [M-MEM]
fn mem_tracker_consume_resp(mem_tracker: Option<&Arc<Tracker>>, resp: &CopResponse) {
    if let Some(t) = mem_tracker {
        let _ = t.consume(-resp.mem_size());
    }
}

/// `util.GetRecoverError(r)`.
// glue: recover() value to error (Go util.GetRecoverError)
fn get_recover_error(p: Box<dyn std::any::Any + Send>) -> Error {
    let msg = p
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
        .unwrap_or_else(|| "panic".to_string());
    Error::plain(msg)
}

impl<S: TiKVStore> CopIteratorWorker<S> {
    /// run is a worker function that get a copTask from channel, handle it and send the
    /// result back.
    // go: copIteratorWorker.run L1 [M-CH]
    async fn run(self) {
        let ch = self.task_ch.clone().expect("worker with a task channel");
        loop {
            let task = { ch.lock().await.recv().await };
            let Some(mut task) = task else { break };
            let sink = match &self.resp_chan {
                Some(tx) => RespSink::Shared(tx.clone()),
                None => RespSink::Task(
                    task.resp_chan
                        .as_ref()
                        .expect("keep-order task has respChan")
                        .tx
                        .clone(),
                ),
            };
            // `close(task.respChan)`: the task's sender is dropped with the task below.
            task.resp_chan = None;
            self.handle_task(task, &sink).await;
            if let Some(tx) = &self.resp_chan {
                // When a task is finished by the worker, send a finCopResp into channel to
                // notify the copIterator that there is a task finished.
                self.send_to_resp_ch_msg(RespMsg::Fin, tx).await;
            }
            drop(sink);
            if self.finished() {
                return;
            }
        }
    }

    // go: copIteratorWorker.sendToRespCh L1 [M-CH]
    async fn send_to_resp_ch(&self, resp: Box<CopResponse>, sink: &RespSink) -> bool {
        match sink {
            RespSink::Shared(tx) => self.send_to_resp_ch_msg(RespMsg::Resp(resp), tx).await,
            RespSink::Task(tx) => {
                let mut finish = self.shared.finish.clone();
                tokio::select! {
                    r = tx.send(resp) => r.is_err(),
                    _ = finish.wait_for(|d| *d) => true,
                }
            }
        }
    }

    // go-part: copIteratorWorker.sendToRespCh
    async fn send_to_resp_ch_msg(&self, msg: RespMsg, tx: &mpsc::Sender<RespMsg>) -> bool {
        let mut finish = self.shared.finish.clone();
        tokio::select! {
            r = tx.send(msg) => r.is_err(),
            _ = finish.wait_for(|d| *d) => true,
        }
    }

    /// Go panics when the tracker's action panics (OOM); the port gets the error from
    /// `Consume` and turns the response into an error response, which is what the panic
    /// becomes after `handleTask`'s recover (L2).
    // go: copIteratorWorker.checkRespOOM L2 (Consume's error replaces the panic recovered by handleTask)
    fn check_resp_oom(&self, resp: &mut CopResponse) {
        if let Some(t) = &self.shared.mem_tracker {
            let consumed = resp.mem_size();
            if let Err(e) = t.consume(consumed) {
                resp.err = Some(e);
            }
        }
    }

    /// handleTask handles single copTask, sends the result to channel, retry automatically
    /// on error.
    // go: copIteratorWorker.handleTask L1 [M-PANIC]
    async fn handle_task(&self, task: CopTask, resp_ch: &RespSink) {
        let r = AssertUnwindSafe(self.handle_task_inner(task, resp_ch))
            .catch_unwind()
            .await;
        if let Err(p) = r {
            log::warn!("copIteratorWork meet panic");
            let resp = Box::new(CopResponse {
                err: Some(get_recover_error(p)),
                ..Default::default()
            });
            // if panic has happened, not checkRespOOM to avoid another panic.
            self.send_to_resp_ch(resp, resp_ch).await;
        }
    }

    // go-part: copIteratorWorker.handleTask
    async fn handle_task_inner(&self, task: CopTask, resp_ch: &RespSink) {
        let mut remain_tasks = std::collections::VecDeque::from([task]);
        let mut backoffermap: HashMap<u64, S::Backoffer> = HashMap::new();
        while let Some(cur_task) = remain_tasks.pop_front() {
            let bo = choose_backoffer(&mut backoffermap, &cur_task, self);
            let result = match self.handle_task_once(bo, cur_task).await {
                Ok(r) => r,
                Err(e) => {
                    let mut resp = Box::new(CopResponse {
                        err: Some(e),
                        ..Default::default()
                    });
                    self.check_resp_oom(&mut resp);
                    self.send_to_resp_ch(resp, resp_ch).await;
                    return;
                }
            };
            let mut remains = Vec::new();
            if let Some(result) = result {
                if let Some(resp) = result.resp {
                    self.send_to_resp_ch(resp, resp_ch).await;
                }
                for resp in result.batch_resp_list {
                    self.send_to_resp_ch(resp, resp_ch).await;
                }
                remains = result.remains;
            }
            if self.finished() {
                break;
            }
            for t in remains.into_iter().rev() {
                remain_tasks.push_front(t);
            }
        }
    }

    /// handleTaskOnce handles single copTask, successful results are send to channel.
    /// If error happened, returns error. If region split or meet lock, returns the remain
    /// tasks.
    // go: copIteratorWorker.handleTaskOnce L1 [M-ERR]
    async fn handle_task_once(
        &self,
        bo: &mut S::Backoffer,
        mut task: CopTask,
    ) -> Result<Option<CopTaskResult>, Error> {
        let req = &self.shared.req;
        if task.paging {
            task.paging_task_idx = self.shared.paging_task_idx.fetch_add(1, Ordering::SeqCst) + 1;
        }

        let mut cop_req = coprocessor::Request {
            tp: req.tp,
            start_ts: req.start_ts,
            data: req.data.clone(),
            ranges: task.ranges.to_pb_ranges(),
            schema_ver: req.schema_var,
            paging_size: task.paging_size,
            tasks: task.to_pb_batch_tasks(),
            connection_id: req.conn_id,
            connection_alias: req.conn_alias.clone(),
            ..Default::default()
        };

        let (cache_key, cache_value) = self.build_cache_key(&task, &mut cop_req);

        if req.replica_read != ReplicaReadLeader || req.is_staleness {
            // Follower / closest / stale reads need replica selection in client-rust
            // (parity table §3.1, RR-5; N11): not supported yet.
            return Err(Error::plain(
                "coprocessor replica read and stale read are not supported yet (client-go parity §3.1)",
            ));
        }
        // N10: the resource group name only (no RU accounting before resource control).
        let rg_name = req.resource_group_name.clone();
        let ctx = cop_req.context.get_or_insert_with(Default::default);
        ctx.isolation_level = isolation_level_to_pb(req.isolation_level) as i32;
        ctx.priority = priority_to_pb(req.priority) as i32;
        ctx.not_fill_cache = req.not_fill_cache;
        ctx.record_time_stat = true;
        ctx.record_scan_stat = true;
        ctx.task_id = req.task_id;
        ctx.resource_control_context = Some(kvrpcpb::ResourceControlContext {
            resource_group_name: rg_name,
            ..Default::default()
        });
        ctx.busy_threshold_ms = task.busy_threshold.as_millis() as u32;
        ctx.buckets_version = task.buckets_ver;
        // `req.InputRequestSource`: client-go writes it into `Context.RequestSource`.
        ctx.request_source = get_request_source(&task.request_source);
        if !task.first_read_type.is_empty() {
            ctx.is_retry_request = true;
        }
        // N9: `ResourceGroupTagger` (Top SQL) is not ported yet (D07.0 T8).
        let mut timeout = Duration::from_nanos(
            tidb_base::config::get_global_config()
                .tikv_client
                .copr_req_timeout
                .0 as u64,
        );
        if task.tikv_client_read_timeout > 0 {
            timeout = Duration::from_millis(task.tikv_client_read_timeout);
            ctx.max_execution_duration_ms = task.tikv_client_read_timeout;
        }
        // N10: no runaway checker. `getEndPointType(task.storeType)` is TiKV here (TiFlash
        // batch cop is DEF.MPP; TiDB tasks are a D06.5 boundary stub).
        let _ = get_end_point_type(task.store_type);
        // `MatchStoreLabels` / `redirect2Replica` need replica read (§3.1): leader only.
        let start_time = Instant::now();
        // `requestRateLimit` (`kv.Request.CoprRequestRateLimit`) is not ported yet.
        let send = self
            .shared
            .kvclient
            .send_req_ctx(bo, cop_req, task.region, timeout)
            .await;
        let (cop_resp, rpc_ctx, store_addr) = match send {
            Ok(r) => r,
            Err(e) => {
                if task.store_type == TiDB {
                    return self.handle_tidb_send_req_err(e, &task).map(Some);
                }
                self.collect_unconsumed_cop_runtime_stats(bo, None);
                return Err(e);
            }
        };

        // Set task.storeAddr field so its task.String() method have the store address
        // information.
        task.store_addr = store_addr;

        let cost_time = start_time.elapsed();
        if cost_time > minLogCopTaskTime {
            self.log_time_cop_task(cost_time, &task, bo, &cop_resp);
        }
        // N9: DistSQLCoprRespBodySize is not recorded.

        let resp = Box::new(CopResponse {
            pb_resp: Some(cop_resp),
            ..Default::default()
        });
        let result = if req.paging.enable {
            self.handle_cop_paging_result(
                bo,
                rpc_ctx.as_ref(),
                resp,
                cache_key,
                cache_value,
                task,
                cost_time,
            )
            .await?
        } else {
            // Handles the response for non-paging copTask.
            self.handle_cop_response(
                bo,
                rpc_ctx.as_ref(),
                resp,
                cache_key,
                cache_value,
                task,
                cost_time,
            )
            .await?
        };
        // `req.ReadType` is set by client-go's replica selector only (replica read): empty.
        Ok(Some(result))
    }

    // go: copIteratorWorker.logTimeCopTask L1
    fn log_time_cop_task(
        &self,
        cost_time: Duration,
        task: &CopTask,
        bo: &S::Backoffer,
        resp: &coprocessor::Response,
    ) {
        let mut log_str = format!(
            "[TIME_COP_PROCESS] resp_time:{:?} txnStartTS:{} region_id:{} store_addr:{}",
            cost_time,
            self.shared.req.start_ts,
            task.region.get_id(),
            task.store_addr
        );
        if bo.get_total_sleep() > minLogBackoffTime {
            log_str += &format!(" backoff_ms:{}", bo.get_total_sleep());
        }
        if let Some(region_err) = &resp.region_error {
            log_str += &format!(" region_err:{region_err:?}");
        }
        let detail_v2 = resp.exec_details_v2.as_ref();
        let detail = resp.exec_details.as_ref();
        let time_detail = detail_v2
            .and_then(|d| d.time_detail.as_ref())
            .or_else(|| detail.and_then(|d| d.time_detail.as_ref()));
        if let Some(td) = time_detail {
            log_str += &format!(" kv_process_ms:{}", td.process_wall_time_ms);
            log_str += &format!(" kv_wait_ms:{}", td.wait_wall_time_ms);
            log_str += &format!(" kv_read_ms:{}", td.kv_read_wall_time_ms);
            if td.process_wall_time_ms <= minLogKVProcessTime {
                log_str = log_str.replacen("TIME_COP_PROCESS", "TIME_COP_WAIT", 1);
            }
        }
        if let Some(sd) = detail_v2.and_then(|d| d.scan_detail_v2.as_ref()) {
            log_str += &format!(" processed_versions:{}", sd.processed_versions);
            log_str += &format!(" total_versions:{}", sd.total_versions);
            log_str += &format!(
                " rocksdb_delete_skipped_count:{}",
                sd.rocksdb_delete_skipped_count
            );
            log_str += &format!(
                " rocksdb_key_skipped_count:{}",
                sd.rocksdb_key_skipped_count
            );
            log_str += &format!(
                " rocksdb_cache_hit_count:{}",
                sd.rocksdb_block_cache_hit_count
            );
            log_str += &format!(" rocksdb_read_count:{}", sd.rocksdb_block_read_count);
            log_str += &format!(" rocksdb_read_byte:{}", sd.rocksdb_block_read_byte);
        } else if let Some(sd) = detail.and_then(|d| d.scan_detail.as_ref()) {
            log_str = append_scan_detail(log_str, "write", sd.write.as_ref());
            log_str = append_scan_detail(log_str, "data", sd.data.as_ref());
            log_str = append_scan_detail(log_str, "lock", sd.lock.as_ref());
        }
        log::info!("{log_str}");
    }

    #[allow(clippy::too_many_arguments)] // Go's signature
    // go: copIteratorWorker.handleCopPagingResult L1 [M-ERR]
    async fn handle_cop_paging_result(
        &self,
        bo: &mut S::Backoffer,
        rpc_ctx: Option<&RPCContext>,
        resp: Box<CopResponse>,
        cache_key: Option<Vec<u8>>,
        cache_value: Option<()>,
        task: CopTask,
        cost_time: Duration,
    ) -> Result<CopTaskResult, Error> {
        let paging_range = resp.pb_resp.as_ref().and_then(|r| r.range.clone());
        let paging_size = task.paging_size;
        // The task goes into handleCopResponse; a successful response hands it back.
        let (mut result, task) = self
            .handle_cop_response_keep(bo, rpc_ctx, resp, cache_key, cache_value, task, cost_time)
            .await?;
        if !result.remains.is_empty() {
            // If there is region error or lock error, keep the paging size and retry.
            for remained_task in &mut result.remains {
                remained_task.paging_size = paging_size;
            }
            return Ok(result);
        }
        let Some(mut task) = task else {
            return Ok(result);
        };
        // only paging requests need to calculate the next ranges
        let Some(paging_range) = paging_range else {
            // If the storage engine doesn't support paging protocol, it should have return
            // all the region data. So we finish here.
            return Ok(result);
        };

        // calculate next ranges and grow the paging size
        task.ranges = calculate_remain(&task.ranges, Some(&paging_range), self.shared.req.desc);
        if task.ranges.is_empty() {
            return Ok(result);
        }

        task.paging_size =
            paging::grow_paging_size(task.paging_size, self.shared.req.paging.max_paging_size);
        result.remains = vec![task];
        Ok(result)
    }

    /// handleCopResponse checks coprocessor Response for region split and lock, returns
    /// more tasks when that happens, or handles the response if no error.
    #[allow(clippy::too_many_arguments)] // Go's signature
    // go: copIteratorWorker.handleCopResponse L2 (the task moves into the result; handleCopPagingResult gets it back through handle_cop_response_keep)
    async fn handle_cop_response(
        &self,
        bo: &mut S::Backoffer,
        rpc_ctx: Option<&RPCContext>,
        resp: Box<CopResponse>,
        cache_key: Option<Vec<u8>>,
        cache_value: Option<()>,
        task: CopTask,
        cost_time: Duration,
    ) -> Result<CopTaskResult, Error> {
        Ok(self
            .handle_cop_response_keep(bo, rpc_ctx, resp, cache_key, cache_value, task, cost_time)
            .await?
            .0)
    }

    /// `handleCopResponse`, returning the task as well when the response was a success
    /// (Go's caller keeps using the task pointer).
    #[allow(clippy::too_many_arguments)] // Go's signature
    // go-part: copIteratorWorker.handleCopResponse
    async fn handle_cop_response_keep(
        &self,
        bo: &mut S::Backoffer,
        rpc_ctx: Option<&RPCContext>,
        mut resp: Box<CopResponse>,
        cache_key: Option<Vec<u8>>,
        cache_value: Option<()>,
        mut task: CopTask,
        cost_time: Duration,
    ) -> Result<(CopTaskResult, Option<CopTask>), Error> {
        let pb = resp.pb_resp.as_mut().expect("response");
        // `UpdateBucketsIfNeeded`: client-rust keeps no buckets (parity §2): no-op.
        if let Some(region_err) = pb.region_error.take() {
            if rpc_ctx.is_some() && task.store_type == TiDB {
                resp.err = Some(tidb_base::errors::errorf(
                    "error: %v",
                    &[Arg::Str(format!("{region_err:?}").into_bytes().into())],
                ));
                self.check_resp_oom(&mut resp);
                return Ok((
                    CopTaskResult {
                        resp: Some(resp),
                        ..Default::default()
                    },
                    None,
                ));
            }
            let err_str = format!(
                "region_id:{}, region_ver:{}, store_type:{}, peer_addr:{}, error:{:?}",
                task.region.get_id(),
                task.region.get_ver(),
                task.store_type.name(),
                task.store_addr,
                region_err
            );
            bo.backoff_region_miss(Error::plain(err_str)).await?;
            let pb_resp = resp.pb_resp.take().unwrap_or_default();
            // We may meet RegionError at the first packet, but not during visiting the stream.
            let cache = self.shared.store.get_region_cache();
            let mut opt = BuildCopTaskOpt::new(&self.shared.req, &cache);
            opt.ignore_tikv_client_read_timeout = true;
            let remains = build_cop_tasks(bo, &task.ranges, &mut opt).await?;
            let batched = task.batch_task_list.take();
            return Ok((
                self.handle_batch_remains_on_err(bo, rpc_ctx, remains, &pb_resp, batched)
                    .await?,
                None,
            ));
        }
        if let Some(lock_err) = pb.locked.take() {
            self.handle_lock_err(bo, Some(lock_err), &task).await?;
            task.meet_lock_fallback = true;
            let pb_resp = resp.pb_resp.take().unwrap_or_default();
            let batched = task.batch_task_list.take();
            return Ok((
                self.handle_batch_remains_on_err(bo, rpc_ctx, vec![task], &pb_resp, batched)
                    .await?,
                None,
            ));
        }
        if !pb.other_error.is_empty() {
            let other_err = pb.other_error.clone();
            let err = tidb_base::errors::errorf(
                "other error: %s",
                &[Arg::Str(other_err.clone().into_bytes().into())],
            );
            log::warn!(
                "other error, txnStartTS: {}, regionID: {}, regionVer: {}, regionConfVer: {}, bucketsVer: {}, latestBucketsVer: {}, rangeNums: {}, storeAddr: {}, error: {}",
                self.shared.req.start_ts,
                task.region.get_id(),
                task.region.get_ver(),
                task.region.get_conf_ver(),
                task.buckets_ver,
                pb.latest_buckets_version,
                task.ranges.len(),
                task.store_addr,
                other_err
            );
            if err.to_string().contains("write conflict") {
                return Err(tidb_kv::error::ErrWriteConflict
                    .fast_gen("%s", vec![Arg::Str(other_err.into_bytes().into())]));
            }
            return Err(err);
        }
        // When the request is using paging API, the `Range` is not nil.
        if let Some(r) = &pb.range {
            resp.start_key = Key(r.start.clone());
        } else if !task.ranges.is_empty() {
            resp.start_key = task.ranges.at(0).start_key;
        }
        self.handle_collect_execution_info(bo, rpc_ctx, &mut resp)?;
        resp.resp_time = cost_time;

        self.handle_cop_cache(&task, &mut resp, cache_key, cache_value)?;

        self.check_resp_oom(&mut resp);
        let batched = task.batch_task_list.take();
        let pb_resp = resp.pb_resp.clone().unwrap_or_default();
        let mut result = CopTaskResult {
            resp: Some(resp),
            ..Default::default()
        };
        let (batch_resp_list, batch_remain_tasks) = self
            .handle_batch_cop_response(bo, rpc_ctx, &pb_resp, batched.unwrap_or_default())
            .await
            .map_err(|(e, _)| e)?;
        result.batch_resp_list = batch_resp_list;
        result.remains = batch_remain_tasks;
        Ok((result, Some(task)))
    }

    // go: copIteratorWorker.handleBatchRemainsOnErr L2 (the caller takes the batched tasks out of the task; Go reads and clears task.batchTaskList here)
    async fn handle_batch_remains_on_err(
        &self,
        bo: &mut S::Backoffer,
        rpc_ctx: Option<&RPCContext>,
        mut remains: Vec<CopTask>,
        resp: &coprocessor::Response,
        batched: Option<BTreeMap<u64, BatchedCopTask>>,
    ) -> Result<CopTaskResult, Error> {
        let Some(batched_tasks) = batched.filter(|b| !b.is_empty()) else {
            return Ok(CopTaskResult {
                remains,
                ..Default::default()
            });
        };
        let (batch_resp_list, remain_tasks) = self
            .handle_batch_cop_response(bo, rpc_ctx, resp, batched_tasks)
            .await
            .map_err(|(e, _)| e)?;
        remains.extend(remain_tasks);
        Ok(CopTaskResult {
            batch_resp_list,
            remains,
            ..Default::default()
        })
    }

    /// handle the batched cop response.
    /// tasks will be changed, so the input tasks should not be used after calling this
    /// function.
    ///
    /// Go returns the responses gathered so far with an error; callers only use the error.
    // go: copIteratorWorker.handleBatchCopResponse L1 [M-ERR, M-MAPORDER]
    #[allow(clippy::collapsible_if)] // Go's nested ifs
    async fn handle_batch_cop_response(
        &self,
        bo: &mut S::Backoffer,
        rpc_ctx: Option<&RPCContext>,
        resp: &coprocessor::Response,
        mut tasks: BTreeMap<u64, BatchedCopTask>,
    ) -> Result<(Vec<Box<CopResponse>>, Vec<CopTask>), BatchErr> {
        if tasks.is_empty() {
            return Ok((Vec::new(), Vec::new()));
        }
        let batched_num = tasks.len();
        let mut busy_threshold_fallback = false;
        let mut remain_tasks: Vec<CopTask> = Vec::new();
        // need Addr for recording details.
        let dummy_rpc_ctx = rpc_ctx.map(|c| RPCContext {
            addr: c.addr.clone(),
            ..Default::default()
        });
        let batch_resps = &resp.batch_responses;
        let mut batch_resp_list: Vec<Box<CopResponse>> = Vec::with_capacity(batch_resps.len());
        for batch_resp in batch_resps {
            let task_id = batch_resp.task_id;
            let Some(batched_task) = tasks.remove(&task_id) else {
                return Err((
                    tidb_base::errors::errorf(
                        "task id %d not found",
                        &[Arg::Uint(task_id, "uint64")],
                    ),
                    batch_resp_list,
                ));
            };
            let mut resp = Box::new(CopResponse {
                pb_resp: Some(coprocessor::Response {
                    data: batch_resp.data.clone(),
                    exec_details_v2: batch_resp.exec_details_v2.clone(),
                    ..Default::default()
                }),
                ..Default::default()
            });
            let mut task = batched_task.task;
            if let Some(region_err) = &batch_resp.region_error {
                let err_str = format!(
                    "region_id:{}, region_ver:{}, store_type:{}, peer_addr:{}, error:{:?}",
                    task.region.get_id(),
                    task.region.get_ver(),
                    task.store_type.name(),
                    task.store_addr,
                    region_err
                );
                if let Err(e) = bo.backoff_region_miss(Error::plain(err_str)).await {
                    return Err((e, batch_resp_list));
                }
                let cache = self.shared.store.get_region_cache();
                let mut opt = BuildCopTaskOpt::new(&self.shared.req, &cache);
                opt.ignore_tikv_client_read_timeout = true;
                match build_cop_tasks(bo, &task.ranges, &mut opt).await {
                    Ok(remains) => remain_tasks.extend(remains),
                    Err(e) => return Err((e, batch_resp_list)),
                }
                continue;
            }
            //TODO: handle locks in batch
            if batch_resp.locked.is_some() {
                // Go passes `resp.pbResp.GetLocked()` of the response just built from the
                // batch response, which has no lock: nil, so nothing is resolved. Kept.
                if let Err(e) = self.handle_lock_err(bo, None, &task).await {
                    return Err((e, batch_resp_list));
                }
                task.meet_lock_fallback = true;
                remain_tasks.push(task);
                continue;
            }
            if !batch_resp.other_error.is_empty() {
                let other_err = batch_resp.other_error.clone();
                let err = tidb_base::errors::errorf(
                    "other error: %s",
                    &[Arg::Str(other_err.clone().into_bytes().into())],
                );
                log::warn!(
                    "other error, txnStartTS: {}, regionID: {}, regionVer: {}, regionConfVer: {}, bucketsVer: {}, rangeNums: {}, storeAddr: {}, error: {}",
                    self.shared.req.start_ts,
                    task.region.get_id(),
                    task.region.get_ver(),
                    task.region.get_conf_ver(),
                    task.buckets_ver,
                    task.ranges.len(),
                    task.store_addr,
                    err
                );
                if err.to_string().contains("write conflict") {
                    return Err((
                        tidb_kv::error::ErrWriteConflict
                            .fast_gen("%s", vec![Arg::Str(other_err.into_bytes().into())]),
                        batch_resp_list,
                    ));
                }
                return Err((err, batch_resp_list));
            }
            if let Err(e) =
                self.handle_collect_execution_info(bo, dummy_rpc_ctx.as_ref(), &mut resp)
            {
                return Err((e, batch_resp_list));
            }
            self.check_resp_oom(&mut resp);
            batch_resp_list.push(resp);
        }
        for (_, t) in std::mem::take(&mut tasks) {
            let task = t.task;
            // when the error is generated by client or a load-based server busy,
            // response is empty by design, skip warning for this case.
            if !batch_resps.is_empty() {
                log::error!(
                    "response of batched task missing, id: {}, txnStartTS: {}, regionID: {}, rangeNums: {}, storeAddr: {}",
                    task.task_id,
                    self.shared.req.start_ts,
                    task.region.get_id(),
                    task.ranges.len(),
                    task.store_addr
                );
            }
            remain_tasks.push(task);
        }
        if let Some(region_err) = &resp.region_error {
            if region_err
                .server_is_busy
                .as_ref()
                .is_some_and(|b| b.estimated_wait_ms > 0)
                && !remain_tasks.is_empty()
            {
                if !batch_resps.is_empty() {
                    return Err((
                        Error::plain(
                            "store batched coprocessor with server is busy error shouldn't contain responses",
                        ),
                        batch_resp_list,
                    ));
                }
                busy_threshold_fallback = true;
                let cache = self.shared.store.get_region_cache();
                let mut handler = new_batch_task_builder(
                    &self.shared.req,
                    &cache,
                    tidb_kv::option::ReplicaReadFollower,
                );
                for mut task in std::mem::take(&mut remain_tasks) {
                    // do not set busy threshold again.
                    task.busy_threshold = Duration::ZERO;
                    if let Err(e) = handler.handle(bo, task).await {
                        return Err((e, batch_resp_list));
                    }
                }
                remain_tasks = handler.build();
            }
        }
        if !busy_threshold_fallback {
            self.shared
                .store_batched_num
                .fetch_add((batched_num - remain_tasks.len()) as u64, Ordering::SeqCst);
            self.shared
                .store_batched_fallback_num
                .fetch_add(remain_tasks.len() as u64, Ordering::SeqCst);
        }
        Ok((batch_resp_list, remain_tasks))
    }

    /// The event callback (`trxevents`) is not ported: the lock is logged at debug level,
    /// Go's behaviour without a callback.
    // go: copIteratorWorker.handleLockErr L1 [M-ERR]
    async fn handle_lock_err(
        &self,
        bo: &mut S::Backoffer,
        lock_err: Option<kvrpcpb::LockInfo>,
        _task: &CopTask,
    ) -> Result<(), Error> {
        let Some(lock_err) = lock_err else {
            return Ok(());
        };
        let _resolve_lock_detail = self.get_lock_resolver_details();
        log::debug!("coprocessor encounters lock, lock: {lock_err:?}");
        self.shared
            .kvclient
            .resolve_lock_and_wait(bo, lock_err)
            .await
    }

    /// The coprocessor cache is D07.4 (exception A3) and not built: no key.
    // go: copIteratorWorker.buildCacheKey L1
    fn build_cache_key(
        &self,
        _task: &CopTask,
        _cop_req: &mut coprocessor::Request,
    ) -> (Option<Vec<u8>>, Option<()>) {
        if self.shared.store.copr_cache.is_some() {
            unreachable!("the coprocessor cache is D07.4");
        }
        (None, None)
    }

    // go: copIteratorWorker.handleCopCache L1
    fn handle_cop_cache(
        &self,
        _task: &CopTask,
        resp: &mut CopResponse,
        cache_key: Option<Vec<u8>>,
        cache_value: Option<()>,
    ) -> Result<(), Error> {
        if resp.pb_resp.as_ref().is_some_and(|r| r.is_cache_hit) {
            if cache_value.is_none() {
                return Err(Error::plain(
                    "Internal error: received illegal TiKV response",
                ));
            }
            unreachable!("no cache value without the coprocessor cache (D07.4)");
        }
        // N9: CoprCacheCounterMiss is a D07.4 metric. No cache key without a cache.
        let _ = cache_key;
        Ok(())
    }

    // go: copIteratorWorker.getLockResolverDetails L1
    fn get_lock_resolver_details(&self) -> Option<()> {
        self.shared.stats.as_ref()?;
        Some(())
    }

    // go: copIteratorWorker.handleCollectExecutionInfo L1
    fn handle_collect_execution_info(
        &self,
        bo: &S::Backoffer,
        rpc_ctx: Option<&RPCContext>,
        resp: &mut CopResponse,
    ) -> Result<(), Error> {
        if self.shared.stats.is_none() {
            return Ok(());
        }
        let mut detail = resp.detail.take().unwrap_or_default();
        let r = self.collect_cop_runtime_stats(&mut detail, bo, rpc_ctx, Some(resp));
        resp.detail = Some(detail);
        r
    }

    // go: copIteratorWorker.collectCopRuntimeStats L1
    #[allow(clippy::collapsible_if)] // Go's nested ifs
    fn collect_cop_runtime_stats(
        &self,
        cop_stats: &mut CopRuntimeStats,
        bo: &S::Backoffer,
        rpc_ctx: Option<&RPCContext>,
        resp: Option<&CopResponse>,
    ) -> Result<(), Error> {
        self.collect_kv_client_runtime_stats(cop_stats, bo, rpc_ctx);
        let Some(resp) = resp else { return Ok(()) };
        let Some(pb) = &resp.pb_resp else {
            return Ok(());
        };
        let d = &mut cop_stats.cop_exec_details;
        if let Some(pb_details) = &pb.exec_details_v2 {
            // Take values in `ExecDetailsV2` first.
            if pb_details.time_detail.is_some() || pb_details.time_detail_v2.is_some() {
                merge_from_time_detail(
                    &mut d.time_detail,
                    pb_details.time_detail_v2.as_ref(),
                    pb_details.time_detail.as_ref(),
                );
            }
            if let Some(sd) = &pb_details.scan_detail_v2 {
                merge_from_scan_detail_v2(&mut d.scan_detail, sd);
            }
        } else if let Some(pb_details) = &pb.exec_details {
            if let Some(td) = &pb_details.time_detail {
                merge_from_time_detail(&mut d.time_detail, None, Some(td));
            }
            if let Some(sd) = &pb_details.scan_detail {
                if let Some(w) = &sd.write {
                    d.scan_detail.processed_keys = w.processed;
                    d.scan_detail.total_keys = w.total;
                }
            }
        }
        // N10: no runaway checker.
        Ok(())
    }

    /// `worker.kvclient.Stats` (client-go `RegionRequestRuntimeStats`) is never set: only
    /// the callee address is recorded (spec §6).
    // go: copIteratorWorker.collectKVClientRuntimeStats L1
    fn collect_kv_client_runtime_stats(
        &self,
        cop_stats: &mut CopRuntimeStats,
        _bo: &S::Backoffer,
        rpc_ctx: Option<&RPCContext>,
    ) {
        if let Some(c) = rpc_ctx {
            cop_stats.cop_exec_details.callee_address = c.addr.clone();
        }
    }

    /// Without client-go's request stats there is nothing unconsumed to collect.
    // go: copIteratorWorker.collectUnconsumedCopRuntimeStats L1
    fn collect_unconsumed_cop_runtime_stats(
        &self,
        _bo: &S::Backoffer,
        _rpc_ctx: Option<&RPCContext>,
    ) {
    }

    // go: copIteratorWorker.handleTiDBSendReqErr L1 [M-ERR]
    fn handle_tidb_send_req_err(&self, err: Error, task: &CopTask) -> Result<CopTaskResult, Error> {
        use protobuf::Message;
        let mut err_code = tidb_base::errno::ErrUnknown as i32;
        let mut err_msg = err.to_string();
        if err.code() == tidb_base::errno::ErrTiKVServerTimeout as isize {
            err_code = tidb_base::errno::ErrTiKVServerTimeout as i32;
            err_msg = format!("TiDB server timeout, address is {}", task.store_addr);
        }
        if err.code() == tidb_base::errno::ErrTiFlashServerTimeout as isize {
            err_code = tidb_base::errno::ErrTiFlashServerTimeout as i32;
            err_msg = format!("TiDB server timeout, address is {}", task.store_addr);
        }
        let mut sel_resp = tipb::SelectResponse::default();
        let mut w = tipb::Error::default();
        w.set_code(err_code);
        w.set_msg(err_msg);
        sel_resp.mut_warnings().push(w);
        let data = sel_resp
            .write_to_bytes()
            .map_err(|e| Error::plain(e.to_string()))?;
        let mut resp = Box::new(CopResponse {
            pb_resp: Some(coprocessor::Response {
                data,
                ..Default::default()
            }),
            detail: Some(Box::default()),
            ..Default::default()
        });
        self.check_resp_oom(&mut resp);
        Ok(CopTaskResult {
            resp: Some(resp),
            ..Default::default()
        })
    }

    /// finished checks the flags and finished channel, it tells whether the worker is
    /// finished.
    // go: copIteratorWorker.finished L1 [M-KILL]
    fn finished(&self) -> bool {
        if let Some(killed) = &self.shared.vars.killed {
            let k = killed.load(Ordering::SeqCst);
            if k != 0 {
                log::info!("a killed signal is received in copIteratorWorker, signal: {k}");
                return true;
            }
        }
        *self.shared.finish.borrow()
    }
}

// go: appendScanDetail L1
fn append_scan_detail(
    mut log_str: String,
    column_family: &str,
    scan_info: Option<&kvrpcpb::ScanInfo>,
) -> String {
    if let Some(s) = scan_info {
        log_str += &format!(" scan_total_{column_family}:{}", s.total);
        log_str += &format!(" scan_processed_{column_family}:{}", s.processed);
    }
    log_str
}

/// The error of `handleBatchCopResponse` with the responses gathered before it.
#[allow(clippy::vec_box)]
type BatchErr = (Error, Vec<Box<CopResponse>>);

/// copErrorResponse returns error when calling Next()
pub struct CopErrorResponse(pub Option<Error>);

impl tidb_kv::kv::Response for CopErrorResponse {
    // go: copErrorResponse.Next L1
    fn next<'a>(
        &'a mut self,
        _ctx: &'a tidb_kv::Context,
    ) -> tidb_kv::kv::KvFuture<'a, Option<Box<dyn tidb_kv::kv::ResultSubset>>> {
        let e = self.0.clone();
        Box::pin(async move {
            match e {
                Some(e) => Err(e),
                None => Ok(None),
            }
        })
    }

    // go: copErrorResponse.Close L1
    fn close(&mut self) -> Result<(), Error> {
        Ok(())
    }
}

/// What `CopClient.Send` returns: the iterator, built and opened at the first `Next`
/// (see [`CopClient::send`]).
pub struct LazyCopResponse<S: TiKVStore> {
    pending: Option<(Request, Variables, tidb_kv::kv::ClientSendOption)>,
    store: Arc<Store<S>>,
    replica_read_seed: u32,
    it: Option<CopIterator<S>>,
    err: Option<Error>,
}

impl<S: TiKVStore> tidb_kv::kv::Response for LazyCopResponse<S> {
    // glue: kv.Response.Next of CopClient.Send's result (build + open on first use)
    fn next<'a>(
        &'a mut self,
        ctx: &'a tidb_kv::Context,
    ) -> tidb_kv::kv::KvFuture<'a, Option<Box<dyn tidb_kv::kv::ResultSubset>>> {
        Box::pin(async move {
            if let Some(e) = &self.err {
                return Err(e.clone());
            }
            if let Some((req, vars, option)) = self.pending.take() {
                let client = CopClient {
                    store: self.store.clone(),
                    replica_read_seed: self.replica_read_seed,
                };
                match client.build_cop_iterator(req, vars, &option).await {
                    Ok(mut it) => {
                        it.open(option.try_cop_lite_worker.as_ref());
                        self.it = Some(it);
                    }
                    Err(e) => {
                        self.err = Some(e.clone());
                        return Err(e);
                    }
                }
            }
            let it = self.it.as_mut().expect("iterator");
            Ok(it
                .next(ctx)
                .await?
                .map(|r| r as Box<dyn tidb_kv::kv::ResultSubset>))
        })
    }

    // glue: kv.Response.Close of CopClient.Send's result
    fn close(&mut self) -> Result<(), Error> {
        if let Some(mut it) = self.it.take() {
            // Close waits for the workers; they stop at their next check of finishCh.
            let _ = it.finish_tx.send(true);
            it.action_on_exceed.close();
            for h in it.handles.drain(..) {
                h.abort();
            }
        }
        Ok(())
    }
}

/// rateLimitAction an OOM Action which is used to control the token if OOM triggered. The
/// token number should be set on initial. Each time the Action is triggered, one token
/// would be destroyed. If the count of the token is less than 2, the action would be
/// delegated to the fallback action.
pub struct RateLimitAction {
    base: tidb_memory::action::BaseOOMAction,
    /// enabled indicates whether the rateLimitAction is permitted to Action.
    enabled: AtomicU32,
    /// totalTokenNum indicates the total token at initial
    total_token_num: usize,
    cond: Mutex<RateLimitCond>,
}

struct RateLimitCond {
    exceeded: bool,
    remaining_token_num: usize,
    /// `sync.Once`: whether `Action` already ran since the last reset.
    once_done: bool,
    trigger_count_for_test: usize,
}

// go: newRateLimitAction L1
pub fn new_rate_limit_action(total_token_number: usize) -> RateLimitAction {
    RateLimitAction {
        base: Default::default(),
        enabled: AtomicU32::new(0),
        total_token_num: total_token_number,
        cond: Mutex::new(RateLimitCond {
            exceeded: false,
            remaining_token_num: total_token_number,
            once_done: false,
            trigger_count_for_test: 0,
        }),
    }
}

impl tidb_memory::ActionOnExceed for RateLimitAction {
    /// Action implements ActionOnExceed.Action
    // go: rateLimitAction.Action L1 [M-LOCK]
    fn action(&self, t: &Arc<Tracker>) -> Result<(), Error> {
        if !self.is_enabled() {
            if let Some(fallback) = self.base.get_fallback() {
                return fallback.action(t);
            }
            return Ok(());
        }
        let mut cond = self.condition_lock();
        if cond.once_done {
            return Ok(());
        }
        cond.once_done = true;
        if cond.remaining_token_num < 2 {
            self.set_enabled(false);
            log::info!(
                "memory exceeds quota, rateLimitAction delegate to fallback action, total token count: {}",
                self.total_token_num
            );
            drop(cond);
            if let Some(fallback) = self.base.get_fallback() {
                return fallback.action(t);
            }
            return Ok(());
        }
        log::info!(
            "memory exceeds quota, destroy one token now., consumed: {}, quota: {}, total token count: {}, remaining token count: {}",
            t.bytes_consumed(),
            t.get_bytes_limit(),
            self.total_token_num,
            cond.remaining_token_num
        );
        cond.exceeded = true;
        cond.trigger_count_for_test += 1;
        Ok(())
    }
    // glue: BaseOOMAction.SetFallback (embedded in Go)
    fn set_fallback(&self, a: Option<tidb_memory::ActionRef>) {
        self.base.set_fallback(a)
    }
    // glue: BaseOOMAction.GetFallback (embedded in Go)
    fn get_fallback(&self) -> Option<tidb_memory::ActionRef> {
        self.base.get_fallback()
    }
    /// GetPriority get the priority of the Action.
    // go: rateLimitAction.GetPriority L1
    fn get_priority(&self) -> i64 {
        tidb_memory::action::DefRateLimitPriority
    }
    // glue: BaseOOMAction.SetFinished (embedded in Go)
    fn set_finished(&self) {
        self.base.set_finished()
    }
    // glue: BaseOOMAction.IsFinished (embedded in Go)
    fn is_finished(&self) -> bool {
        self.base.is_finished()
    }
}

impl RateLimitAction {
    /// destroyTokenIfNeeded will check the `exceed` flag after copWorker finished one task.
    /// If the exceed flag is true and there is no token been destroyed before, one token
    /// will be destroyed, or the token would be return back.
    // go: rateLimitAction.destroyTokenIfNeeded L1 [M-LOCK]
    pub fn destroy_token_if_needed(&self, return_token: impl FnOnce()) {
        if !self.is_enabled() {
            return_token();
            return;
        }
        let mut cond = self.condition_lock();
        if !cond.exceeded {
            drop(cond);
            return_token();
            return;
        }
        // If actionOnExceed has been triggered and there is no token have been destroyed
        // before, destroy one token.
        cond.remaining_token_num -= 1;
        cond.exceeded = false;
        cond.once_done = false;
    }

    // go: rateLimitAction.conditionLock L1 [M-LOCK]
    fn condition_lock(&self) -> std::sync::MutexGuard<'_, RateLimitCond> {
        self.cond.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The guard's drop is Go's `conditionUnlock`.
    // go: rateLimitAction.conditionUnlock L1 [M-LOCK]
    fn condition_unlock(guard: std::sync::MutexGuard<'_, RateLimitCond>) {
        drop(guard);
    }

    // go: rateLimitAction.close L1 [M-LOCK]
    pub fn close(&self) {
        if !self.is_enabled() {
            return;
        }
        self.set_enabled(false);
        let mut cond = self.condition_lock();
        cond.exceeded = false;
        Self::condition_unlock(cond);
        self.base.set_finished();
    }

    // go: rateLimitAction.setEnabled L1
    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(u32::from(enabled), Ordering::SeqCst);
    }

    // go: rateLimitAction.isEnabled L1
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::SeqCst) > 0
    }
}
