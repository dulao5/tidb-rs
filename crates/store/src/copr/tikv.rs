//! The client-go surface `pkg/store/copr` calls (`tikv.RegionCache`, `tikv.KeyLocation`,
//! `tikv.RegionVerID`, `tikv.RPCContext`, `tikv.Store`).
//!
//! client-go is replaced by the client-rust fork (L3, ADR 0008) and is not ported function
//! by function. This module is the boundary: the copr port is written against the
//! [`TiKVRegionCache`] trait, which the client-rust adapter (D07.2 part 2) and the test
//! layouts implement. The value types and their small methods follow client-go c3b5079
//! (`internal/locate/region_cache.go`), because copr's task splitting reads them.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::time::Duration;

use tidb_base::errors::Error;
use tidb_kv::KeyRange;
use tidb_kv::option::ReplicaReadType;
use tikv_client::proto::{coprocessor, kvrpcpb, metapb};

/// `bytes.Compare`.
// go-std: bytes.Compare L1
fn bytes_compare(a: &[u8], b: &[u8]) -> isize {
    match a.cmp(b) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    }
}

/// `tikv.RegionVerID`: a region's id with its epoch, the identity a cop task is sent to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct RegionVerID {
    pub id: u64,
    pub conf_ver: u64,
    pub ver: u64,
}

impl RegionVerID {
    /// `tikv.NewRegionVerID`.
    // go-l3: ADR-0008 client-go internal/locate NewRegionVerID
    pub fn new(id: u64, conf_ver: u64, ver: u64) -> RegionVerID {
        RegionVerID { id, conf_ver, ver }
    }

    // go-l3: ADR-0008 client-go internal/locate RegionVerID.GetID
    pub fn get_id(&self) -> u64 {
        self.id
    }

    // go-l3: ADR-0008 client-go internal/locate RegionVerID.GetVer
    pub fn get_ver(&self) -> u64 {
        self.ver
    }

    // go-l3: ADR-0008 client-go internal/locate RegionVerID.GetConfVer
    pub fn get_conf_ver(&self) -> u64 {
        self.conf_ver
    }
}

/// `tikv.KeyLocation`: the region a key range falls in, as the region cache knows it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct KeyLocation {
    pub region: RegionVerID,
    pub start_key: Vec<u8>,
    pub end_key: Vec<u8>,
    /// `*metapb.Buckets`; `None` is Go's nil.
    pub buckets: Option<metapb::Buckets>,
}

/// `tikv.Bucket` (`locate.Bucket`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Bucket {
    pub start_key: Vec<u8>,
    pub end_key: Vec<u8>,
}

/// client-go `locate.contains`.
// go-l3: ADR-0008 client-go internal/locate contains
fn contains(start_key: &[u8], end_key: &[u8], key: &[u8]) -> bool {
    bytes_compare(start_key, key) <= 0 && (bytes_compare(key, end_key) < 0 || end_key.is_empty())
}

impl KeyLocation {
    /// Contains checks if key is in [StartKey, EndKey).
    // go-l3: ADR-0008 client-go internal/locate KeyLocation.Contains
    pub fn contains(&self, key: &[u8]) -> bool {
        bytes_compare(&self.start_key, key) <= 0
            && (bytes_compare(key, &self.end_key) < 0 || self.end_key.is_empty())
    }

    /// GetBucketVersion gets the bucket version of the region. If the region doesn't contain
    /// buckets, returns 0.
    // go-l3: ADR-0008 client-go internal/locate KeyLocation.GetBucketVersion
    pub fn get_bucket_version(&self) -> u64 {
        match &self.buckets {
            None => 0,
            Some(b) => b.version,
        }
    }

    /// LocateBucket handles with a type of edge case of locateBucket that returns nil.
    /// There are two cases where locateBucket returns nil:
    /// Case one is that the key neither does not belong to any bucket nor does not belong
    /// to the region.
    /// Case two is that the key belongs to the region but not any bucket.
    /// LocateBucket will not return nil in the case two.
    /// Specifically, when the key is in [KeyLocation.StartKey, first Bucket key), the result
    /// returned by locateBucket will be nil as there's no bucket containing this key.
    /// LocateBucket will return Bucket{KeyLocation.StartKey, first Bucket key} as the result.
    /// Also, when the key is in [last Bucket key, KeyLocation.EndKey), LocateBucket will
    /// return Bucket{last Bucket key, KeyLocation.EndKey}.
    ///
    /// Go dereferences `l.Buckets` (nil when the region has no buckets) after the key was
    /// found in the region: copr only calls it on locations that have buckets.
    // go-l3: ADR-0008 client-go internal/locate KeyLocation.LocateBucket
    pub fn locate_bucket(&self, key: &[u8]) -> Option<Bucket> {
        let bucket = self.locate_bucket_inner(key);
        if bucket.is_some() {
            return bucket;
        }
        if !self.contains(key) {
            return None;
        }
        let keys = &self
            .buckets
            .as_ref()
            .expect("invalid memory address or nil pointer dereference")
            .keys;
        let counts = keys.len();
        if counts == 0 {
            return Some(Bucket {
                start_key: self.start_key.clone(),
                end_key: self.end_key.clone(),
            });
        }
        let first_bucket_key = &keys[0];
        if bytes_compare(key, first_bucket_key) < 0 {
            return Some(Bucket {
                start_key: self.start_key.clone(),
                end_key: first_bucket_key.clone(),
            });
        }
        let last_bucket_key = &keys[counts - 1];
        if bytes_compare(last_bucket_key, key) <= 0 {
            return Some(Bucket {
                start_key: last_bucket_key.clone(),
                end_key: self.end_key.clone(),
            });
        }
        log::info!("Unreachable place");
        panic!("Unreachable");
    }

    /// locateBucket returns the bucket the key is located. It returns nil if the key is
    /// outside the bucket.
    // go-l3: ADR-0008 client-go internal/locate KeyLocation.locateBucket
    fn locate_bucket_inner(&self, key: &[u8]) -> Option<Bucket> {
        let empty = Vec::new();
        let keys = self.buckets.as_ref().map(|b| &b.keys).unwrap_or(&empty);
        // Go: `searchLen := len(keys) - 1`; with no keys it is -1 and sort.Search(-1) is 0.
        let search_len = keys.len() as isize - 1;
        let i = sort_search(search_len, |i| bytes_compare(key, &keys[i]) < 0);

        if i == 0
            || (i == search_len
                && !keys[search_len as usize].is_empty()
                && bytes_compare(key, &keys[search_len as usize]) >= 0)
        {
            return None;
        }
        Some(Bucket {
            start_key: keys[(i - 1) as usize].clone(),
            end_key: keys[i as usize].clone(),
        })
    }
}

impl Bucket {
    /// Contains checks whether the key is in the Bucket.
    // go-l3: ADR-0008 client-go internal/locate Bucket.Contains
    pub fn contains(&self, key: &[u8]) -> bool {
        contains(&self.start_key, &self.end_key, key)
    }
}

/// `sort.Search(n, f)`: the smallest index in [0, n) at which f is true, n if none.
// go-std: sort.Search L1
pub(crate) fn sort_search(n: isize, mut f: impl FnMut(usize) -> bool) -> isize {
    let (mut i, mut j) = (0isize, n);
    while i < j {
        let h = ((i + j) as usize >> 1) as isize;
        if !f(h as usize) {
            i = h + 1;
        } else {
            j = h;
        }
    }
    i
}

/// The part of `tikv.RPCContext` (and of its `*tikv.Store`) that copr reads when it batches
/// cop tasks by store (`RegionCache.BuildBatchTask`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RPCContext {
    /// `Region`.
    pub region: RegionVerID,
    /// `Peer`.
    pub peer: metapb::Peer,
    /// `Addr`.
    pub addr: String,
    /// `Store.StoreID()`.
    pub store_id: u64,
    /// `Store.EstimatedWaitTime()`.
    pub store_estimated_wait_time: Duration,
}

/// The `tikv.RegionCache` methods copr calls, with client-go's semantics. Implemented by
/// the client-rust adapter; tests implement it over a fixed region layout.
///
/// Errors are already TiDB errors (copr wraps every call in `derr.ToTiDBErr`, which is the
/// identity on an error that is not a client-go one).
pub trait TiKVRegionCache: Send + Sync + 'static {
    /// The `*retry.Backoffer` the calls take (TiDB's `backoff.Backoffer` passes its
    /// `TiKVBackoffer()`).
    type Backoffer: Backoffer;

    /// `RegionCache.BatchLocateKeyRanges(bo, ranges, opts...)` with
    /// `WithNeedRegionHasLeaderPeer()` when `need_leader` and `WithNeedBuckets()` when
    /// `need_buckets`: the locations of the regions the ranges cover, in key order, each once.
    // go-l3: ADR-0008 client-go internal/locate RegionCache.BatchLocateKeyRanges
    fn batch_locate_key_ranges(
        &self,
        bo: &mut Self::Backoffer,
        key_ranges: &[KeyRange],
        need_leader: bool,
        need_buckets: bool,
    ) -> impl Future<Output = Result<Vec<KeyLocation>, Error>> + Send;

    /// `RegionCache.GetTiKVRPCContext(bo, id, replicaRead, followerStoreSeed)`; `Ok(None)`
    /// is Go's nil context (the region is not cached any more).
    // go-l3: ADR-0008 client-go internal/locate RegionCache.GetTiKVRPCContext
    fn get_tikv_rpc_context(
        &self,
        bo: &mut Self::Backoffer,
        id: RegionVerID,
        replica_read: ReplicaReadType,
        follower_store_seed: u32,
    ) -> impl Future<Output = Result<Option<RPCContext>, Error>> + Send;
}

/// `tikv.Variables`: the per-session values the client reads.
#[derive(Clone, Debug)]
pub struct Variables {
    /// `BackoffWeight`: multiplies every backoff budget (`tidb_backoff_weight`, default 2).
    pub backoff_weight: i64,
    /// `Killed`: non-zero once the statement is killed.
    pub killed: Option<Arc<AtomicU32>>,
}

impl Default for Variables {
    // go-l3: ADR-0008 client-go kv NewVariables
    fn default() -> Self {
        Variables {
            backoff_weight: 2,
            killed: None,
        }
    }
}

/// TiDB's `backoff.Backoffer` (`pkg/store/driver/backoff`, block D07.1) over client-go's
/// `retry.Backoffer`: the methods copr calls. Errors are already TiDB errors.
pub trait Backoffer: Send + Sync + 'static {
    /// `bo.Backoff(tikv.BoRegionMiss(), err)`.
    // go-l3: ADR-0008 client-go config/retry Backoffer.Backoff
    fn backoff_region_miss(&mut self, err: Error)
    -> impl Future<Output = Result<(), Error>> + Send;
    /// `bo.BackoffWithMaxSleepTxnLockFast(maxSleepMs, err)`.
    // go-l3: ADR-0008 client-go config/retry Backoffer.BackoffWithMaxSleepTxnLockFast
    fn backoff_with_max_sleep_txn_lock_fast(
        &mut self,
        max_sleep_ms: i64,
        err: Error,
    ) -> impl Future<Output = Result<(), Error>> + Send;
    /// `bo.GetTotalSleep()` (ms).
    // go-l3: ADR-0008 client-go config/retry Backoffer.GetTotalSleep
    fn get_total_sleep(&self) -> i64;
}

/// client-go `txnsnapshot.ClientHelper` bound to one iterator's lock sets
/// (`NewClientHelper(store, &it.resolvedLocks, &it.committedLocks, false)`).
pub trait ClientHelper: Send + Sync + 'static {
    type Backoffer: Backoffer;

    /// `ClientHelper.SendReqCtx(bo, req, regionID, timeout, et, directStoreAddr)`: one
    /// coprocessor RPC with client-go's retries; region errors the caller must handle come
    /// back in the response. Returns `(resp, rpcCtx, storeAddr)`.
    // go-l3: ADR-0008 client-go txnkv/txnsnapshot ClientHelper.SendReqCtx
    fn send_req_ctx(
        &self,
        bo: &mut Self::Backoffer,
        req: coprocessor::Request,
        region: RegionVerID,
        timeout: Duration,
    ) -> impl Future<Output = Result<(coprocessor::Response, Option<RPCContext>, String), Error>> + Send;

    /// `ClientHelper.ResolveLocksWithOpts` for the locks of `lock_info` (a shared-lock
    /// wrapper through its members), then `bo.BackoffWithMaxSleepTxnLockFast(msBeforeExpired)`
    /// while a lock is alive: the two steps of `copIteratorWorker.handleLockErr` after the
    /// event callback (client-rust resolves and waits in one call).
    // go-l3: ADR-0008 client-go txnkv/txnsnapshot ClientHelper.ResolveLocksWithOpts
    fn resolve_lock_and_wait(
        &self,
        bo: &mut Self::Backoffer,
        lock_info: kvrpcpb::LockInfo,
    ) -> impl Future<Output = Result<(), Error>> + Send;
}

/// The `tikv.KVStore` methods copr calls besides the region cache.
pub trait TiKVStore: TiKVRegionCache {
    type Helper: ClientHelper<Backoffer = Self::Backoffer>;

    /// A client helper for one iterator reading at `start_ts`; `resolved_start_ts`: whether
    /// the iterator's resolved-lock set starts with `start_ts` (`!req.IsStaleness`).
    // go-l3: ADR-0008 client-go txnkv/txnsnapshot NewClientHelper
    fn new_client_helper(&self, start_ts: u64, resolved_start_ts: bool) -> Self::Helper;

    /// `backoff.NewBackofferWithVars(ctx, maxSleep, vars)`: `max_sleep_ms × backoff weight`.
    // go-l3: ADR-0008 client-go config/retry NewBackofferWithVars
    fn new_backoffer(&self, max_sleep_ms: i64, vars: &Variables) -> Self::Backoffer;

    /// `KVStore.CheckVisibility(startTS)`: fails when the GC safe point passed `start_ts`.
    // go-l3: ADR-0008 client-go tikv KVStore.CheckVisibility
    fn check_visibility(&self, start_ts: u64) -> Result<(), Error>;
}

/// client-go `util.RateLimit`: a token bucket of fixed capacity.
pub struct RateLimit {
    sem: tokio::sync::Semaphore,
    capacity: usize,
}

impl RateLimit {
    // go-l3: ADR-0008 client-go util NewRateLimit
    pub fn new(n: usize) -> RateLimit {
        if n == 0 {
            panic!("invalid rate limit capacity");
        }
        RateLimit {
            sem: tokio::sync::Semaphore::new(n),
            capacity: n,
        }
    }

    /// `GetToken(done)`: waits for a token; `true` (exit) when `done` fires first.
    // go-l3: ADR-0008 client-go util RateLimit.GetToken
    pub async fn get_token(&self, done: &mut tokio::sync::watch::Receiver<bool>) -> bool {
        if *done.borrow() {
            return true;
        }
        tokio::select! {
            permit = self.sem.acquire() => {
                permit.expect("rate limit semaphore closed").forget();
                false
            }
            _ = done.wait_for(|d| *d) => true,
        }
    }

    // go-l3: ADR-0008 client-go util RateLimit.PutToken
    pub fn put_token(&self) {
        self.sem.add_permits(1);
    }

    // go-l3: ADR-0008 client-go util RateLimit.GetCapacity
    pub fn get_capacity(&self) -> usize {
        self.capacity
    }
}

/// client-go `util.RequestSource.GetRequestSource`.
// go-l3: ADR-0008 client-go util RequestSource.GetRequestSource
pub fn get_request_source(r: &tidb_kv::context::RequestSource) -> String {
    let mut source = "unknown".to_string();
    let mut origin = "external";
    if r.request_source_type.is_empty() && r.explicit_request_source_type.is_empty() {
        return source;
    }
    if r.request_source_internal {
        origin = "internal";
    }
    let mut label_list = vec![origin.to_string()];
    if !r.request_source_type.is_empty() {
        source = r.request_source_type.clone();
    }
    label_list.push(source);
    if !r.explicit_request_source_type.is_empty()
        && r.explicit_request_source_type != r.request_source_type
    {
        label_list.push(r.explicit_request_source_type.clone());
    }
    label_list.join("_")
}

/// client-go `TimeDetail.MergeFromTimeDetail`.
// go-l3: ADR-0008 client-go util TimeDetail.MergeFromTimeDetail
pub fn merge_from_time_detail(
    td: &mut tidb_execdetails::clientgo::TimeDetail,
    v2: Option<&kvrpcpb::TimeDetailV2>,
    v1: Option<&kvrpcpb::TimeDetail>,
) {
    if let Some(v2) = v2 {
        td.wait_time += v2.wait_wall_time_ns as i64;
        td.process_time += v2.process_wall_time_ns as i64;
        td.suspend_time += v2.process_suspend_wall_time_ns as i64;
        td.kv_read_wall_time += v2.kv_read_wall_time_ns as i64;
        td.total_rpc_wall_time += v2.total_rpc_wall_time_ns as i64;
    } else if let Some(v1) = v1 {
        td.wait_time += v1.wait_wall_time_ms as i64 * 1_000_000;
        td.process_time += v1.process_wall_time_ms as i64 * 1_000_000;
        td.kv_read_wall_time += v1.kv_read_wall_time_ms as i64 * 1_000_000;
        td.total_rpc_wall_time += v1.total_rpc_wall_time_ns as i64;
    }
}

/// client-go `ScanDetail.MergeFromScanDetailV2`.
// go-l3: ADR-0008 client-go util ScanDetail.MergeFromScanDetailV2
pub fn merge_from_scan_detail_v2(
    sd: &mut tidb_execdetails::clientgo::ScanDetail,
    v2: &kvrpcpb::ScanDetailV2,
) {
    sd.total_keys += v2.total_versions as i64;
    sd.processed_keys += v2.processed_versions as i64;
    sd.processed_keys_size += v2.processed_versions_size as i64;
    sd.rocksdb_delete_skipped_count += v2.rocksdb_delete_skipped_count;
    sd.rocksdb_key_skipped_count += v2.rocksdb_key_skipped_count;
    sd.rocksdb_block_cache_hit_count += v2.rocksdb_block_cache_hit_count;
    sd.rocksdb_block_read_count += v2.rocksdb_block_read_count;
    sd.rocksdb_block_read_byte += v2.rocksdb_block_read_byte;
    sd.rocksdb_block_read_duration += v2.rocksdb_block_read_nanos as i64;
    sd.get_snapshot_duration += v2.get_snapshot_nanos as i64;
}

/// client-go `tikvrpc.EndpointType`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EndpointType {
    TiKV,
    TiFlash,
    TiDB,
    TiFlashCompute,
}
