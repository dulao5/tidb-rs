//! go-file: pkg/store/copr/coprocessor.go
//!
//! The coprocessor client. Part 1 (D07.2 spec §2): the cop task, task building (by
//! partition, region and bucket, with the store-batch builder), small-task concurrency,
//! the paging range arithmetic and the request-field conversions. The iterator, its workers
//! and response handling are part 2.

// Package-level consts keep their Go names (mapping rule M-GONAME).
#![allow(non_upper_case_globals)]

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

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
use super::tikv::{RegionVerID, TiKVRegionCache};
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
/// Part 1 only defines the value the task channels carry; `detail` (`*CopRuntimeStats`),
/// the accessors and the memory accounting come with the iterator (part 2).
#[derive(Debug, Default)]
pub struct CopResponse {
    pub pb_resp: Option<coprocessor::Response>,
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
