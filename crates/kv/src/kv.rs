//! go-file: pkg/kv/kv.go
//!
//! The storage interfaces of the read path, `KeyRanges`, `Request` and the KV helpers.
//!
//! Interfaces (spec §3.2):
//! - Go interfaces become object-safe traits (`dyn Snapshot`, `dyn Storage`, ...): the
//!   store driver (D07.1), cached-table and temporary-table interceptors and the test mocks
//!   implement them, and callers hold them as trait objects as Go holds interface values.
//! - Calls that may do I/O are async (N3 3.1 item 6: no sync/async boundary for KV
//!   access); a trait method returns a boxed future ([`KvFuture`]), the object-safe form of
//!   an async method. `Iterator::next` is async for the same reason (a scanner fetches the
//!   next batch).
//! - Only the methods of the read path are declared. The write side of `Transaction`
//!   (`Set`, `Delete`, `LockKeys`, fair locking, assertions, memdb checkpoints, pipelined
//!   flush), `Mutator`, `MemBuffer`, `RetrieverMutator`, `AssertionProto`,
//!   `FairLockingController`, `SplittableStore` and `EtcdBackend` are the write path done by
//!   Go TiDB (tracker scoping, rules.py) and are not declared.
//! - `SetOption(opt int, val any)` takes the option value as `Box<dyn Any>` ([`OptionValue`]).
//! - Go's `map[string]ValueEntry` results are `HashMap<Vec<u8>, ValueEntry>` (M-BYTES: keys
//!   are raw bytes).

// Go identifiers for the constants (M-GONAME).
#![allow(non_upper_case_globals)]

use core::iter::Iterator as _;
use std::any::Any;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::AtomicU64;
use std::time::Duration;

use futures::future::BoxFuture;
use tidb_base::Error;
use tidb_base::config;
use tidb_base::kv::StoreType;
use tidb_memory::Tracker;

use crate::context::Context;
use crate::context::RequestSource;
use crate::error::ErrNotExist;
use crate::key::Key;
use crate::key::KeyRange;
use crate::key::bytes_compare;
use crate::option::ReplicaReadType;
use crate::txn_scope_var::GlobalTxnScope;
use crate::version::Version;

/// The future of an interface method that may do I/O.
pub type KvFuture<'a, T> = BoxFuture<'a, Result<T, Error>>;

/// The value of `SetOption(opt int, val any)`.
pub type OptionValue = Box<dyn Any + Send + Sync>;

/// UnCommitIndexKVFlag uses to indicate the index key/value is no need to commit.
/// This is used in the situation of the index key/value was unchanged when do update.
/// Usage:
/// 1. For non-unique index: normally, the index value is '0'.
///    Change the value to '1' indicate the index key/value is no need to commit.
/// 2. For unique index: normally, the index value is the record handle ID, 8 bytes.
///    Append UnCommitIndexKVFlag to the value indicate the index key/value is no need to commit.
pub const UnCommitIndexKVFlag: u8 = b'1';

/// TxnEntrySizeLimit is limit of single entry size (len(key) + len(value)).
pub static TxnEntrySizeLimit: AtomicU64 = AtomicU64::new(config::DefTxnEntrySizeLimit);
/// TxnTotalSizeLimit is limit of the sum of all entry size.
pub static TxnTotalSizeLimit: AtomicU64 = AtomicU64::new(config::DefTxnTotalSizeLimit);

/// ValueEntry represents a value with its commit TS (client-go `kv.ValueEntry`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ValueEntry {
    pub value: Vec<u8>,
    pub commit_ts: u64,
}

/// NewValueEntry creates a ValueEntry.
// go: NewValueEntry L1
pub fn new_value_entry(value: Vec<u8>, commit_ts: u64) -> ValueEntry {
    // client-go kv.NewValueEntry
    ValueEntry { value, commit_ts }
}

/// GetOption is the option for Get (client-go `kv.GetOption`). v8.5.8's client-go has one
/// option, `WithReturnCommitTS`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GetOption {
    ReturnCommitTS,
}

/// BatchGetOption is the option for BatchGet (client-go `kv.BatchGetOption`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BatchGetOption {
    ReturnCommitTS,
}

/// BatchGetToGetOptions converts []BatchGetOption to []GetOption.
// go: BatchGetToGetOptions L1
pub fn batch_get_to_get_options(options: &[BatchGetOption]) -> Vec<GetOption> {
    if options.is_empty() {
        return Vec::new();
    }
    // client-go kv.BatchGetToGetOptions: each option is also a GetOption.
    options
        .iter()
        .map(|o| match o {
            BatchGetOption::ReturnCommitTS => GetOption::ReturnCommitTS,
        })
        .collect()
}

/// Getter is the interface for the Get method.
pub trait Getter: Send + Sync {
    // glue: Go interface method Getter.Get
    /// Get gets the value for key k from kv store.
    /// If corresponding kv pair does not exist, it returns nil and ErrNotExist.
    fn get<'a>(
        &'a self,
        ctx: &'a Context,
        k: &'a Key,
        options: &'a [GetOption],
    ) -> KvFuture<'a, ValueEntry>;
}

/// GetValue gets the value for key k from kv store.
// go: GetValue L1
pub async fn get_value(
    ctx: &Context,
    getter: &(impl Getter + ?Sized),
    k: &Key,
) -> Result<Vec<u8>, Error> {
    getter.get(ctx, k, &[]).await.map(|entry| entry.value)
}

/// Retriever is the interface wraps the basic Get and Seek methods.
pub trait Retriever: Getter {
    // glue: Go interface method Retriever.Iter
    /// Iter creates an Iterator positioned on the first entry that k <= entry's key.
    /// If such entry is not found, it returns an invalid Iterator with no error.
    /// It yields only keys that < upperBound. If upperBound is nil, it means the upperBound
    /// is unbounded. The Iterator must be Closed after use.
    fn iter<'a>(&'a self, k: &'a Key, upper_bound: &'a Key)
    -> KvFuture<'a, Box<dyn Iterator + 'a>>;

    // glue: Go interface method Retriever.IterReverse
    /// IterReverse creates a reversed Iterator positioned on the first entry which key is
    /// less than k. The returned iterator will iterate from greater key to smaller key.
    /// If k is nil, the returned iterator will be positioned at the last key.
    /// It yields only keys that >= lowerBound. If lowerBound is nil, it means the
    /// lowerBound is unbounded.
    fn iter_reverse<'a>(
        &'a self,
        k: &'a Key,
        lower_bound: &'a Key,
    ) -> KvFuture<'a, Box<dyn Iterator + 'a>>;
}

/// EmptyIterator is an iterator without any entry
#[derive(Debug, Default)]
pub struct EmptyIterator;

static EMPTY_KEY: Key = Key(Vec::new());

impl Iterator for EmptyIterator {
    /// Valid returns true if the current iterator is valid.
    // go: EmptyIterator.Valid L1
    fn valid(&self) -> bool {
        false
    }

    /// Key returns the current key. Go returns nil.
    // go: EmptyIterator.Key L1
    fn key(&self) -> &Key {
        &EMPTY_KEY
    }

    /// Value returns the current value. Go returns nil.
    // go: EmptyIterator.Value L1
    fn value(&self) -> &[u8] {
        &[]
    }

    /// Next goes the next position. Always return error for this iterator
    // go: EmptyIterator.Next L1
    fn next(&mut self) -> KvFuture<'_, ()> {
        Box::pin(async { Err(tidb_base::errors::new("iterator is invalid")) })
    }

    /// Close closes the iterator.
    // go: EmptyIterator.Close L1
    fn close(&mut self) {}
}

/// EmptyRetriever is a retriever without any entry
#[derive(Debug, Default)]
pub struct EmptyRetriever;

impl Getter for EmptyRetriever {
    /// Get gets the value for key k from kv store. Always return nil for this retriever
    // go: EmptyRetriever.Get L1
    fn get<'a>(
        &'a self,
        _ctx: &'a Context,
        _k: &'a Key,
        _options: &'a [GetOption],
    ) -> KvFuture<'a, ValueEntry> {
        Box::pin(async { Err(ErrNotExist.clone()) })
    }
}

impl Retriever for EmptyRetriever {
    /// Iter creates an Iterator. Always return EmptyIterator for this retriever
    // go: EmptyRetriever.Iter L1
    fn iter<'a>(
        &'a self,
        _k: &'a Key,
        _upper_bound: &'a Key,
    ) -> KvFuture<'a, Box<dyn Iterator + 'a>> {
        Box::pin(async { Ok(Box::new(EmptyIterator) as Box<dyn Iterator>) })
    }

    /// IterReverse creates an reversed Iterator. Always return EmptyIterator for this
    /// retriever
    // go: EmptyRetriever.IterReverse L1
    fn iter_reverse<'a>(
        &'a self,
        _k: &'a Key,
        _lower_bound: &'a Key,
    ) -> KvFuture<'a, Box<dyn Iterator + 'a>> {
        Box::pin(async { Ok(Box::new(EmptyIterator) as Box<dyn Iterator>) })
    }
}

/// StagingHandle is the reference of a staging buffer.
pub type StagingHandle = isize;

/// InvalidStagingHandle is an invalid handler, MemBuffer will check handler to ensure
/// safety.
pub const InvalidStagingHandle: StagingHandle = 0;
/// LastActiveStagingHandle is an special handler which always point to the last active
/// staging buffer.
pub const LastActiveStagingHandle: StagingHandle = -1;

/// Transaction defines the interface for operations inside a Transaction.
/// This is not thread safe.
///
/// The read-path subset (module docs).
#[allow(clippy::len_without_is_empty)] // Go has Len and no IsEmpty
pub trait Transaction: Retriever + BatchGetter {
    // glue: Go interface method Transaction.Size
    /// Size returns sum of keys and values length.
    fn size(&self) -> isize;
    // glue: Go interface method Transaction.Len
    /// Len returns the number of entries in the DB.
    fn len(&self) -> isize;
    // glue: Go interface method Transaction.Commit
    /// Commit commits the transaction operations to KV store.
    fn commit<'a>(&'a mut self, ctx: &'a Context) -> KvFuture<'a, ()>;
    // glue: Go interface method Transaction.Rollback
    /// Rollback undoes the transaction operations to KV store.
    fn rollback(&mut self) -> Result<(), Error>;
    // glue: Go interface method Transaction.String
    /// String implements fmt.Stringer interface.
    fn string(&self) -> String;
    // glue: Go interface method Transaction.SetOption
    /// SetOption sets an option with a value, when val is nil, uses the default
    /// value of this option.
    fn set_option(&mut self, opt: isize, val: OptionValue);
    // glue: Go interface method Transaction.GetOption
    /// GetOption returns the option
    fn get_option(&self, opt: isize) -> Option<&(dyn Any + Send + Sync)>;
    // glue: Go interface method Transaction.IsReadOnly
    /// IsReadOnly checks if the transaction has only performed read operations.
    fn is_read_only(&self) -> bool;
    // glue: Go interface method Transaction.StartTS
    /// StartTS returns the transaction start timestamp.
    fn start_ts(&self) -> u64;
    // glue: Go interface method Transaction.Valid
    /// Valid returns if the transaction is valid.
    /// A transaction become invalid after commit or rollback.
    fn valid(&self) -> bool;
    // glue: Go interface method Transaction.GetSnapshot
    /// GetSnapshot returns the Snapshot binding to this transaction.
    fn get_snapshot(&self) -> Arc<dyn Snapshot>;
    // glue: Go interface method Transaction.IsPessimistic
    /// IsPessimistic returns true if it is pessimistic.
    fn is_pessimistic(&self) -> bool;
}

/// Client is used to send request to KV layer.
pub trait Client: Send + Sync {
    // glue: Go interface method Client.Send
    /// Send sends request to KV layer, returns a Response.
    fn send(
        &self,
        ctx: &Context,
        req: &Request,
        vars: Option<OptionValue>,
        option: &ClientSendOption,
    ) -> Box<dyn Response>;

    // glue: Go interface method Client.IsRequestTypeSupported
    /// IsRequestTypeSupported checks if reqType and subType is supported.
    fn is_request_type_supported(&self, req_type: i64, sub_type: i64) -> bool;
}

/// ClientSendOption wraps options during Client Send.
///
/// `EventCb` (`trxevents`) and `TiFlashReplicaRead` (`util/tiflash`, MPP scope) are added
/// with their blocks.
#[derive(Clone, Default)]
pub struct ClientSendOption {
    pub session_mem_tracker: Option<Arc<Tracker>>,
    pub enabled_rate_limit_action: bool,
    pub enable_collect_execution_info: bool,
    pub append_warning: Option<Arc<dyn Fn(Error) + Send + Sync>>,
    /// TryCopLiteWorker indicates whether try to use coprocessor lite worker.
    pub try_cop_lite_worker: Option<Arc<AtomicU32>>,
}

/// ReqTypes.
pub const ReqTypeSelect: i64 = 101;
pub const ReqTypeIndex: i64 = 102;
pub const ReqTypeDAG: i64 = 103;
pub const ReqTypeAnalyze: i64 = 104;
pub const ReqTypeChecksum: i64 = 105;

pub const ReqSubTypeBasic: i64 = 0;
pub const ReqSubTypeDesc: i64 = 10000;
pub const ReqSubTypeGroupBy: i64 = 10001;
pub const ReqSubTypeTopN: i64 = 10002;
pub const ReqSubTypeSignature: i64 = 10003;
pub const ReqSubTypeAnalyzeIdx: i64 = 10004;
pub const ReqSubTypeAnalyzeCol: i64 = 10005;

/// KeyRanges wrap some kinds of ranges.
///
/// Go's `rowCountHints` is a nil-able slice of nil-able slices: only its length is read,
/// and a partition's hints may be nil (passed as nil to `ForEachPartitionWithErr`'s
/// callback), so the port keeps `Vec<Option<Vec<isize>>>`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KeyRanges {
    ranges: Vec<Vec<KeyRange>>,
    row_count_hints: Vec<Option<Vec<isize>>>,
    is_partitioned: bool,
}

/// NewPartitionedKeyRanges constructs a new RequestRange for partitioned table.
// go: NewPartitionedKeyRanges L1
pub fn new_partitioned_key_ranges(ranges: Vec<Vec<KeyRange>>) -> KeyRanges {
    new_partitioned_key_ranges_with_hints(ranges, Vec::new())
}

/// NewNonPartitionedKeyRanges constructs a new RequestRange for a non-partitioned table.
// go: NewNonPartitionedKeyRanges L1
pub fn new_non_partitioned_key_ranges(ranges: Vec<KeyRange>) -> KeyRanges {
    new_non_paritioned_key_ranges_with_hint(ranges, None)
}

/// NewPartitionedKeyRangesWithHints constructs a new RequestRange for partitioned table
/// with row count hint.
// go: NewPartitionedKeyRangesWithHints L1
pub fn new_partitioned_key_ranges_with_hints(
    ranges: Vec<Vec<KeyRange>>,
    hints: Vec<Option<Vec<isize>>>,
) -> KeyRanges {
    KeyRanges {
        ranges,
        row_count_hints: hints,
        is_partitioned: true,
    }
}

/// NewNonParitionedKeyRangesWithHint constructs a new RequestRange for a non partitioned
/// table with rou count hint.
// go: NewNonParitionedKeyRangesWithHint L1
pub fn new_non_paritioned_key_ranges_with_hint(
    ranges: Vec<KeyRange>,
    hints: Option<Vec<isize>>,
) -> KeyRanges {
    let mut rr = KeyRanges {
        ranges: vec![ranges],
        row_count_hints: Vec::new(),
        is_partitioned: false,
    };
    if let Some(hints) = hints {
        rr.row_count_hints = vec![Some(hints)];
    }
    rr
}

/// `slices.IsSortedFunc`.
// go-std: slices.IsSortedFunc L1
fn is_sorted_func<T>(x: &[T], mut cmp: impl FnMut(&T, &T) -> isize) -> bool {
    for i in (1..x.len()).rev() {
        if cmp(&x[i], &x[i - 1]) < 0 {
            return false;
        }
    }
    true
}

/// `slices.SortFunc`: Go sorts with pdqsort, which is not stable. `slices.SortFunc`
/// (`zsortanyfunc.go`) and `sort.Sort` (`zsortinterface.go`) are generated from the same
/// template, so [`tidb_base::gostd::sort::sort`] with `cmp(a, b) < 0` as `Less` makes the
/// same comparisons in the same order, and equal elements end up in Go's order.
// go-std: slices.SortFunc L1
fn sort_func<T>(x: &mut [T], mut cmp: impl FnMut(&T, &T) -> isize) {
    tidb_base::gostd::sort::sort(x, |a, b| cmp(a, b) < 0);
}

impl KeyRanges {
    /// FirstPartitionRange returns the the result of first range.
    /// We may use some func to generate ranges for both partitioned table and non
    /// partitioned table. This method provides a way to fallback to non-partitioned ranges.
    // go: KeyRanges.FirstPartitionRange L1
    pub fn first_partition_range(&self) -> &[KeyRange] {
        if self.ranges.is_empty() {
            return &[];
        }
        &self.ranges[0]
    }

    /// SetToNonPartitioned set the status to non-partitioned.
    // go: KeyRanges.SetToNonPartitioned L1
    pub fn set_to_non_partitioned(&mut self) -> Result<(), Error> {
        if self.ranges.len() > 1 {
            return Err(tidb_base::errors::errorf(
                "you want to change the partitioned ranges to non-partitioned ranges",
                &[],
            ));
        }
        self.is_partitioned = false;
        Ok(())
    }

    /// AppendSelfTo appends itself to another slice.
    // go: KeyRanges.AppendSelfTo L1
    pub fn append_self_to(&self, mut ranges: Vec<KeyRange>) -> Vec<KeyRange> {
        for r in &self.ranges {
            ranges.extend_from_slice(r);
        }
        ranges
    }

    /// SortByFunc sorts each partition's ranges.
    /// Since the ranges are sorted in most cases, we check it first.
    // go: KeyRanges.SortByFunc L1
    pub fn sort_by_func(&mut self, mut sort_func: impl FnMut(&KeyRange, &KeyRange) -> isize) {
        if !is_sorted_func(&self.ranges, |i, j| {
            if i.is_empty() || j.is_empty() {
                return -1;
            }
            sort_func(&i[0], &j[0])
        }) {
            sort_func_outer(&mut self.ranges, &mut sort_func);
        }
        for i in 0..self.ranges.len() {
            if !is_sorted_func(&self.ranges[i], &mut sort_func) {
                sort_func_impl(&mut self.ranges[i], &mut sort_func);
            }
        }
    }

    /// ForEachPartitionWithErr runs the func for each partition with an error check.
    // go: KeyRanges.ForEachPartitionWithErr L1
    pub fn for_each_partition_with_err(
        &self,
        mut the_func: impl FnMut(&[KeyRange], Option<&[isize]>) -> Result<(), Error>,
    ) -> Result<(), Error> {
        for i in 0..self.ranges.len() {
            let mut hints: Option<&[isize]> = None;
            if self.row_count_hints.len() > i {
                hints = self.row_count_hints[i].as_deref();
            }
            the_func(&self.ranges[i], hints)?;
        }
        Ok(())
    }

    /// PartitionNum returns how many partition is involved in the ranges.
    // go: KeyRanges.PartitionNum L1
    pub fn partition_num(&self) -> isize {
        self.ranges.len() as isize
    }

    /// IsFullySorted checks whether the ranges are sorted inside partition and each
    /// partition is also sorated.
    // go: KeyRanges.IsFullySorted L1
    pub fn is_fully_sorted(&self) -> bool {
        let sorted_by_partition = is_sorted_func(&self.ranges, |i, j| {
            if i.is_empty() || j.is_empty() {
                return -1;
            }
            bytes_compare(&i[0].start_key, &j[0].start_key)
        });
        if !sorted_by_partition {
            return false;
        }
        for ranges in &self.ranges {
            if !is_sorted_func(ranges, |i, j| bytes_compare(&i.start_key, &j.start_key)) {
                return false;
            }
        }
        true
    }

    /// The ranges of every partition (Go: the unexported field, read by tests in package
    /// kv).
    // glue: field access
    pub fn ranges(&self) -> &[Vec<KeyRange>] {
        &self.ranges
    }

    /// Whether the ranges are partitioned (Go: the unexported field).
    // glue: field access
    pub fn is_partitioned(&self) -> bool {
        self.is_partitioned
    }
}

/// The outer `slices.SortFunc` of `SortByFunc`, comparing partitions by their first range.
// glue: closure of KeyRanges.SortByFunc (the outer comparator)
fn sort_func_outer(
    ranges: &mut [Vec<KeyRange>],
    sort_func: &mut impl FnMut(&KeyRange, &KeyRange) -> isize,
) {
    sort_func_impl(ranges, |i: &Vec<KeyRange>, j: &Vec<KeyRange>| {
        if i.is_empty() {
            return -1;
        }
        if j.is_empty() {
            return 1;
        }
        sort_func(&i[0], &j[0])
    });
}

// glue: name for slices.SortFunc inside SortByFunc (its parameter shadows sort_func)
fn sort_func_impl<T>(x: &mut [T], cmp: impl FnMut(&T, &T) -> isize) {
    sort_func(x, cmp);
}

/// IsoLevel is the transaction's isolation level.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IsoLevel(pub isize);

/// SI stands for 'snapshot isolation'.
pub const SI: IsoLevel = IsoLevel(0);
/// RC stands for 'read committed'.
pub const RC: IsoLevel = IsoLevel(1);
/// RCCheckTS stands for 'read consistency' read with ts check.
pub const RCCheckTS: IsoLevel = IsoLevel(2);

/// Priority value for transaction priority.
pub const PriorityNormal: isize = 0;
pub const PriorityLow: isize = 1;
pub const PriorityHigh: isize = 2;

/// CoprRequestAdjuster is used to check and adjust a copr request according to specific
/// rules. return true if the request is changed.
pub type CoprRequestAdjuster = Arc<dyn Fn(&mut Request, isize) -> bool + Send + Sync>;

/// Request represents a kv request.
///
/// Fields whose types belong to blocks not ported yet are added with them (spec §4):
/// `CoprRequestRateLimit` (client-go `util.RateLimit`, D07.2), `MatchStoreLabels`
/// (kvproto `metapb.StoreLabel`, D07.1), `ResourceGroupTagger` (`ResourceGroupTagBuilder`,
/// waits D02.2 / D01.11 / D07.1), `RunawayChecker` (`resourcegroup`, D05.x).
#[derive(Clone, Default)]
pub struct Request {
    /// Tp is the request type.
    pub tp: i64,
    pub start_ts: u64,
    pub data: Vec<u8>,

    /// KeyRanges makes sure that the request is sent first by partition then by region.
    /// When the table is small, it's possible that multiple partitions are in the same
    /// region.
    pub key_ranges: Option<KeyRanges>,

    /// For PartitionTableScan used by tiflash.
    pub partition_id_and_ranges: Vec<PartitionIDAndRanges>,

    /// Concurrency is 1, if it only sends the request to a single storage unit when
    /// ResponseIterator.Next is called. If concurrency is greater than 1, the request will
    /// be sent to multiple storage units concurrently.
    pub concurrency: isize,
    /// IsolationLevel is the isolation level, default is SI.
    pub isolation_level: IsoLevel,
    /// Priority is the priority of this KV request, its value may be PriorityNormal/
    /// PriorityLow/PriorityHigh.
    pub priority: isize,
    /// memTracker is used to trace and control memory usage in co-processor layer.
    pub mem_tracker: Option<Arc<Tracker>>,
    /// KeepOrder is true, if the response should be returned in order.
    pub keep_order: bool,
    /// Desc is true, if the request is sent in descending order.
    pub desc: bool,
    /// NotFillCache makes this request do not touch the LRU cache of the underlying
    /// storage.
    pub not_fill_cache: bool,
    /// ReplicaRead is used for reading data from replicas, only follower is supported at
    /// this time.
    pub replica_read: ReplicaReadType,
    /// StoreType represents this request is sent to the which type of store.
    pub store_type: StoreType,
    /// Cacheable is true if the request can be cached. Currently only deterministic DAG
    /// requests can be cached.
    pub cacheable: bool,
    /// SchemaVer is for any schema-ful storage to validate schema correctness if necessary.
    pub schema_var: i64,
    /// BatchCop indicates whether send batch coprocessor request to tiflash.
    pub batch_cop: bool,
    /// TaskID is an unique ID for an execution of a statement
    pub task_id: u64,
    /// TiDBServerID is the specified TiDB serverID to execute request. `0` means all TiDB
    /// instances.
    pub tidb_server_id: u64,
    /// TxnScope is the scope of the txn
    pub txn_scope: String,
    /// ReadReplicaScope is the scope of the read replica.
    pub read_replica_scope: String,
    /// IsStaleness indicates whether the request read staleness data
    pub is_staleness: bool,
    /// ClosestReplicaReadAdjuster used to adjust a copr request.
    pub closest_replica_read_adjuster: Option<CoprRequestAdjuster>,
    /// Paging indicates whether the request is a paging request.
    pub paging: Paging,
    /// RequestSource indicates whether the request is an internal request.
    pub request_source: RequestSource,
    /// StoreBatchSize indicates the batch size of coprocessor in the same store.
    pub store_batch_size: isize,
    /// ResourceGroupName is the name of the bind resource group.
    pub resource_group_name: String,
    /// LimitSize indicates whether the request is scan and limit
    pub limit_size: u64,
    /// StoreBusyThreshold is the threshold for the store to return ServerIsBusy
    pub store_busy_threshold: Duration,
    /// TiKVClientReadTimeout is the timeout of kv read request
    pub tikv_client_read_timeout: u64,
    /// ConnID stores the session connection id.
    pub conn_id: u64,
    /// ConnAlias stores the session connection alias.
    pub conn_alias: String,
}

/// The `Paging` field of [`Request`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Paging {
    pub enable: bool,
    /// MinPagingSize is used when Enable is true.
    pub min_paging_size: u64,
    /// MaxPagingSize is used when Enable is true.
    pub max_paging_size: u64,
}

/// PartitionIDAndRanges used by PartitionTableScan in tiflash.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PartitionIDAndRanges {
    pub id: i64,
    pub key_ranges: Vec<KeyRange>,
}

/// GlobalReplicaScope indicates the default replica scope for tidb to request
pub const GlobalReplicaScope: &str = GlobalTxnScope;

/// ResultSubset represents a result subset from a single storage unit.
/// TODO: Find a better interface for ResultSubset that can reuse bytes.
pub trait ResultSubset: Send {
    // glue: Go interface method ResultSubset.GetData
    /// GetData gets the data.
    fn get_data(&self) -> &[u8];
    // glue: Go interface method ResultSubset.GetStartKey
    /// GetStartKey gets the start key.
    fn get_start_key(&self) -> &Key;
    // glue: Go interface method ResultSubset.MemSize
    /// MemSize returns how many bytes of memory this result use for tracing memory usage.
    fn mem_size(&self) -> i64;
    // glue: Go interface method ResultSubset.RespTime
    /// RespTime returns the response time for the request.
    fn resp_time(&self) -> Duration;
}

/// Response represents the response returned from KV layer.
pub trait Response: Send {
    // glue: Go interface method Response.Next
    /// Next returns a resultSubset from a single storage unit.
    /// When full result set is returned, nil is returned.
    fn next<'a>(&'a mut self, ctx: &'a Context) -> KvFuture<'a, Option<Box<dyn ResultSubset>>>;
    // glue: Go interface method Response.Close
    /// Close response.
    fn close(&mut self) -> Result<(), Error>;
}

/// Snapshot defines the interface for the snapshot fetched from KV store.
pub trait Snapshot: Retriever + BatchGetter {
    // glue: Go interface method Snapshot.SetOption
    /// SetOption sets an option with a value, when val is nil, uses the default
    /// value of this option. Only ReplicaRead is supported for snapshot
    fn set_option(&self, opt: isize, val: OptionValue);
}

/// SnapshotInterceptor is used to intercept snapshot's read operation
pub trait SnapshotInterceptor: Send + Sync {
    // glue: Go interface method SnapshotInterceptor.OnGet
    /// OnGet intercepts Get operation for Snapshot
    fn on_get<'a>(
        &'a self,
        ctx: &'a Context,
        snap: &'a dyn Snapshot,
        k: &'a Key,
        options: &'a [GetOption],
    ) -> KvFuture<'a, ValueEntry>;
    // glue: Go interface method SnapshotInterceptor.OnBatchGet
    /// OnBatchGet intercepts BatchGet operation for Snapshot
    fn on_batch_get<'a>(
        &'a self,
        ctx: &'a Context,
        snap: &'a dyn Snapshot,
        keys: &'a [Key],
        options: &'a [BatchGetOption],
    ) -> KvFuture<'a, HashMap<Vec<u8>, ValueEntry>>;
    // glue: Go interface method SnapshotInterceptor.OnIter
    /// OnIter intercepts Iter operation for Snapshot
    fn on_iter<'a>(
        &'a self,
        snap: &'a dyn Snapshot,
        k: &'a Key,
        upper_bound: &'a Key,
    ) -> KvFuture<'a, Box<dyn Iterator + 'a>>;
    // glue: Go interface method SnapshotInterceptor.OnIterReverse
    /// OnIterReverse intercepts IterReverse operation for Snapshot
    fn on_iter_reverse<'a>(
        &'a self,
        snap: &'a dyn Snapshot,
        k: &'a Key,
        lower_bound: &'a Key,
    ) -> KvFuture<'a, Box<dyn Iterator + 'a>>;
}

/// BatchGetter is the interface for BatchGet.
pub trait BatchGetter: Send + Sync {
    // glue: Go interface method BatchGetter.BatchGet
    /// BatchGet gets a batch of values.
    fn batch_get<'a>(
        &'a self,
        ctx: &'a Context,
        keys: &'a [Key],
        options: &'a [BatchGetOption],
    ) -> KvFuture<'a, HashMap<Vec<u8>, ValueEntry>>;
}

/// BatchGetValue gets a batch of values from BatchGetter.
// go: BatchGetValue L1
pub async fn batch_get_value(
    ctx: &Context,
    getter: &(impl BatchGetter + ?Sized),
    keys: &[Key],
) -> Result<HashMap<Vec<u8>, Vec<u8>>, Error> {
    let entries = getter.batch_get(ctx, keys, &[]).await?;
    let mut values = HashMap::with_capacity(entries.len());
    for (k, entry) in entries {
        values.insert(k, entry.value);
    }
    Ok(values)
}

/// TxnOption configures `Storage.Begin` (client-go `tikv.TxnOption`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TxnOption {
    /// `tikv.WithStartTS`.
    StartTS(u64),
    /// `tikv.WithTxnScope`.
    TxnScope(String),
}

/// Storage defines the interface for storage.
/// Isolation should be at least SI(SNAPSHOT ISOLATION)
///
/// The read-path subset; `GetMPPClient` (MPP scope), `GetOracle`, `GetLockWaits`,
/// `GetCodec`, `ShowStatus` and `SupportDeleteRange` are added with the store driver
/// (D07.1) and the blocks that call them.
pub trait Storage: Send + Sync {
    // glue: Go interface method Storage.Begin
    /// Begin a global transaction
    fn begin<'a>(&'a self, opts: &'a [TxnOption]) -> KvFuture<'a, Box<dyn Transaction>>;
    // glue: Go interface method Storage.GetSnapshot
    /// GetSnapshot gets a snapshot that is able to read any data which data is <= the
    /// given ver. if ver is MaxVersion or > current max committed version, we will use
    /// current version for this snapshot.
    fn get_snapshot(&self, ver: Version) -> Arc<dyn Snapshot>;
    // glue: Go interface method Storage.GetClient
    /// GetClient gets a client instance.
    fn get_client(&self) -> Arc<dyn Client>;
    // glue: Go interface method Storage.Close
    /// Close store
    fn close(&self) -> Result<(), Error>;
    // glue: Go interface method Storage.UUID
    /// UUID return a unique ID which represents a Storage.
    fn uuid(&self) -> String;
    // glue: Go interface method Storage.CurrentVersion
    /// CurrentVersion returns current max committed version with the given txnScope
    /// (local or global).
    fn current_version<'a>(&'a self, txn_scope: &'a str) -> KvFuture<'a, Version>;
    // glue: Go interface method Storage.Name
    /// Name gets the name of the storage engine
    fn name(&self) -> String;
    // glue: Go interface method Storage.Describe
    /// Describe returns of brief introduction of the storage
    fn describe(&self) -> String;
    // glue: Go interface method Storage.GetMinSafeTS
    /// GetMinSafeTS return the minimal SafeTS of the storage with given txnScope.
    fn get_min_safe_ts(&self, txn_scope: &str) -> u64;
}

/// FnKeyCmp is the function for iterator the keys
pub type FnKeyCmp<'a> = &'a mut dyn FnMut(&Key) -> bool;

/// Iterator is the interface for a iterator on KV store.
pub trait Iterator: Send {
    // glue: Go interface method Iterator.Valid
    fn valid(&self) -> bool;
    // glue: Go interface method Iterator.Key
    fn key(&self) -> &Key;
    // glue: Go interface method Iterator.Value
    fn value(&self) -> &[u8];
    // glue: Go interface method Iterator.Next
    fn next(&mut self) -> KvFuture<'_, ()>;
    // glue: Go interface method Iterator.Close
    fn close(&mut self);
}
