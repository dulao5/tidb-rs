//! The client-go surface `pkg/store/copr` calls (`tikv.RegionCache`, `tikv.KeyLocation`,
//! `tikv.RegionVerID`, `tikv.RPCContext`, `tikv.Store`).
//!
//! client-go is replaced by the client-rust fork (L3, ADR 0008) and is not ported function
//! by function. This module is the boundary: the copr port is written against the
//! [`TiKVRegionCache`] trait, which the client-rust adapter (D07.2 part 2) and the test
//! layouts implement. The value types and their small methods follow client-go c3b5079
//! (`internal/locate/region_cache.go`), because copr's task splitting reads them.

use std::future::Future;
use std::time::Duration;

use tidb_base::errors::Error;
use tidb_kv::KeyRange;
use tidb_kv::option::ReplicaReadType;
use tikv_client::proto::metapb;

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
pub trait TiKVRegionCache: Send + Sync {
    /// The `*retry.Backoffer` the calls take (TiDB's `backoff.Backoffer` passes its
    /// `TiKVBackoffer()`).
    type Backoffer: Send;

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
