//! go-file: pkg/store/copr/region_cache.go
//!
//! `RegionCache` wraps the client's region cache and splits key ranges by region and by
//! region bucket. The client's cache is the [`TiKVRegionCache`] boundary (ADR 0008).

// Package-level consts keep their Go names (mapping rule M-GONAME).
#![allow(non_upper_case_globals)]

use std::sync::Arc;

use tidb_base::errors::{self, Error};
use tidb_base::gofmt::Arg;
use tidb_kv::kv::Request;
use tidb_kv::option::{ReplicaReadLeader, ReplicaReadType};
use tidb_kv::{Key, KeyRange};
use tikv_client::proto::{coprocessor, metapb};

use super::coprocessor::{BatchedCopTask, CopTask};
use super::key_ranges::{KeyRanges, new_key_ranges};
use super::tikv::{KeyLocation, TiKVRegionCache};

/// RegionCache wraps tikv.RegionCache.
pub struct RegionCache<C: TiKVRegionCache> {
    pub cache: Arc<C>,
}

impl<C: TiKVRegionCache> Clone for RegionCache<C> {
    // glue: copying the RegionCache wrapper (Go copies the struct holding the pointer)
    fn clone(&self) -> Self {
        RegionCache {
            cache: self.cache.clone(),
        }
    }
}

/// NewRegionCache returns a new RegionCache.
// go: NewRegionCache L1
pub fn new_region_cache<C: TiKVRegionCache>(rc: Arc<C>) -> RegionCache<C> {
    RegionCache { cache: rc }
}

/// LocationKeyRanges wraps a real Location in PD and its logical ranges info.
#[derive(Clone, Debug)]
pub struct LocationKeyRanges {
    /// Location is the real location in PD.
    pub location: Arc<KeyLocation>,
    /// Ranges is the logic ranges the current Location contains.
    pub ranges: KeyRanges,
}

/// UnspecifiedLimit means no limit.
pub const UnspecifiedLimit: isize = -1;

impl LocationKeyRanges {
    // go: LocationKeyRanges.getBucketVersion L1
    pub(crate) fn get_bucket_version(&self) -> u64 {
        self.location.get_bucket_version()
    }

    /// splitKeyRangeByBuckets splits ranges in the same location by buckets and returns a
    /// LocationKeyRanges array.
    // go: LocationKeyRanges.splitKeyRangesByBuckets L1
    pub(crate) fn split_key_ranges_by_buckets(&self) -> Vec<LocationKeyRanges> {
        if self
            .location
            .buckets
            .as_ref()
            .is_none_or(|b| b.keys.is_empty())
        {
            return vec![self.clone()];
        }

        let mut ranges = self.ranges.clone();
        let loc = &self.location;
        let mut res = Vec::new();
        while !ranges.is_empty() {
            // ranges must be in loc.region, so the bucket returned by loc.LocateBucket is
            // guaranteed to be not nil
            let bucket = loc
                .locate_bucket(&ranges.ref_at(0).start_key)
                .expect("invalid memory address or nil pointer dereference");

            // Iterate to the first range that is not complete in the bucket.
            let mut r = KeyRange::default();
            let mut i = 0;
            while i < ranges.len() {
                r = ranges.at(i);
                if !(bucket.contains(&r.end_key) || bucket.end_key == r.end_key.0) {
                    break;
                }
                i += 1;
            }
            // All rest ranges belong to the same bucket.
            if i == ranges.len() {
                res.push(LocationKeyRanges {
                    location: self.location.clone(),
                    ranges,
                });
                break;
            }

            if bucket.contains(&r.start_key) {
                // Part of r is not in the bucket. We need to split it.
                let mut task_ranges = ranges.slice(0, i);
                task_ranges.last = Some(KeyRange {
                    start_key: r.start_key.clone(),
                    end_key: Key(bucket.end_key.clone()),
                });
                res.push(LocationKeyRanges {
                    location: self.location.clone(),
                    ranges: task_ranges,
                });

                ranges = ranges.slice(i + 1, ranges.len());
                ranges.first = Some(KeyRange {
                    start_key: Key(bucket.end_key.clone()),
                    end_key: r.end_key.clone(),
                });
            } else {
                // ranges[i] is not in the bucket.
                let task_ranges = ranges.slice(0, i);
                res.push(LocationKeyRanges {
                    location: self.location.clone(),
                    ranges: task_ranges,
                });
                ranges = ranges.slice(i, ranges.len());
            }
        }
        res
    }
}

impl<C: TiKVRegionCache> RegionCache<C> {
    /// SplitRegionRanges gets the split ranges from pd region.
    // go: RegionCache.SplitRegionRanges L1 [M-ERR]
    pub async fn split_region_ranges(
        &self,
        bo: &mut C::Backoffer,
        key_ranges: Vec<KeyRange>,
        limit: isize,
    ) -> Result<Vec<KeyRange>, Error> {
        let ranges = new_key_ranges(key_ranges);

        let locations = self
            .split_key_ranges_by_locations(bo, ranges, limit, true, false)
            .await?;
        let mut ret = Vec::new();
        for loc in &locations {
            for i in 0..loc.ranges.len() {
                ret.push(loc.ranges.at(i));
            }
        }
        Ok(ret)
    }

    // go: RegionCache.splitKeyRangesByLocation L1
    fn split_key_ranges_by_location(
        &self,
        loc: &Arc<KeyLocation>,
        mut ranges: KeyRanges,
        res: &mut Vec<LocationKeyRanges>,
    ) -> (KeyRanges, bool) {
        // Iterate to the first range that is not complete in the region.
        let mut r = KeyRange::default();
        let mut i = 0;
        while i < ranges.len() {
            r = ranges.at(i);
            if !(loc.contains(&r.end_key) || loc.end_key == r.end_key.0) {
                break;
            }
            i += 1;
        }
        // All rest ranges belong to the same region.
        if i == ranges.len() {
            res.push(LocationKeyRanges {
                location: loc.clone(),
                ranges: ranges.clone(),
            });
            return (ranges, true);
        }
        if loc.contains(&r.start_key) {
            // Part of r is not in the region. We need to split it.
            let mut task_ranges = ranges.slice(0, i);
            task_ranges.last = Some(KeyRange {
                start_key: r.start_key.clone(),
                end_key: Key(loc.end_key.clone()),
            });
            res.push(LocationKeyRanges {
                location: loc.clone(),
                ranges: task_ranges,
            });
            ranges = ranges.slice(i + 1, ranges.len());
            ranges.first = Some(KeyRange {
                start_key: Key(loc.end_key.clone()),
                end_key: r.end_key.clone(),
            });
        } else {
            // rs[i] is not in the region.
            if i > 0 {
                let task_ranges = ranges.slice(0, i);
                res.push(LocationKeyRanges {
                    location: loc.clone(),
                    ranges: task_ranges,
                });
                ranges = ranges.slice(i, ranges.len());
            }
        }
        (ranges, false)
    }

    /// SplitKeyRangesByLocations splits the KeyRanges by logical info in the cache.
    /// The buckets in the returned LocationKeyRanges are empty, regardless of whether the
    /// region is split by bucket.
    // go: RegionCache.SplitKeyRangesByLocations L1 [M-ERR]
    pub async fn split_key_ranges_by_locations(
        &self,
        bo: &mut C::Backoffer,
        mut ranges: KeyRanges,
        limit: isize,
        need_leader: bool,
        buckets: bool,
    ) -> Result<Vec<LocationKeyRanges>, Error> {
        if limit == 0 || ranges.is_empty() {
            return Ok(Vec::new());
        }

        let mut kv_ranges = Vec::with_capacity(ranges.len());
        for i in 0..ranges.len() {
            kv_ranges.push(KeyRange {
                start_key: ranges.ref_at(i).start_key.clone(),
                end_key: ranges.ref_at(i).end_key.clone(),
            });
        }
        let locs = self
            .cache
            .batch_locate_key_ranges(bo, &kv_ranges, need_leader, buckets)
            .await?;
        let locs: Vec<Arc<KeyLocation>> = locs.into_iter().map(Arc::new).collect();

        let mut res_cap = locs.len();
        if limit != UnspecifiedLimit {
            res_cap = res_cap.min(limit as usize);
        }
        let mut res = Vec::with_capacity(res_cap);

        let mut next_loc_index = 0;
        while !ranges.is_empty() {
            if limit != UnspecifiedLimit && res.len() as isize >= limit {
                break;
            }

            if next_loc_index >= locs.len() {
                return Err(errors::errorf(
                    "Unexpected loc index %d, which should less than %d",
                    &[
                        Arg::Int(next_loc_index as i64, "int"),
                        Arg::Int(locs.len() as i64, "int"),
                    ],
                ));
            }

            let loc = &locs[next_loc_index];
            // For the last loc.
            if next_loc_index == locs.len() - 1 {
                res.push(LocationKeyRanges {
                    location: loc.clone(),
                    ranges,
                });
                break;
            }
            next_loc_index += 1;

            let is_break;
            (ranges, is_break) = self.split_key_ranges_by_location(loc, ranges, &mut res);
            if is_break {
                break;
            }
        }
        Ok(res)
    }

    /// SplitKeyRangesByBuckets splits the KeyRanges by buckets information in the cache.
    /// If regions don't have buckets, it's equal to SplitKeyRangesByLocations.
    // go: RegionCache.SplitKeyRangesByBuckets L1 [M-ERR]
    pub async fn split_key_ranges_by_buckets(
        &self,
        bo: &mut C::Backoffer,
        ranges: KeyRanges,
    ) -> Result<Vec<LocationKeyRanges>, Error> {
        let locs = self
            .split_key_ranges_by_locations(bo, ranges, UnspecifiedLimit, false, true)
            .await?;
        let mut res = Vec::with_capacity(locs.len());
        for loc in &locs {
            res.extend(loc.split_key_ranges_by_buckets());
        }
        Ok(res)
    }

    /// BuildBatchTask fetches store and peer info for cop task, wrap it as
    /// `batchedCopTask`.
    // go: RegionCache.BuildBatchTask L2 (takes the task by value and hands it back when it is not batched; Go shares the pointer)
    pub(crate) async fn build_batch_task(
        &self,
        bo: &mut C::Backoffer,
        req: &Request,
        task: CopTask,
        replica_read: ReplicaReadType,
    ) -> Result<Result<BatchedCopTask, CopTask>, Error> {
        if replica_read != ReplicaReadLeader {
            return Ok(Err(task));
        }

        // Go passes `options.GetTiKVReplicaReadType(replicaRead)`: the adapter maps it.
        let rpc_context = self
            .cache
            .get_tikv_rpc_context(bo, task.region, replica_read, 0)
            .await?;

        // fallback to non-batch path
        let Some(rpc_context) = rpc_context else {
            return Ok(Err(task));
        };

        // when leader is busy, we don't batch the cop task to allow the load balance to work.
        if rpc_context.store_estimated_wait_time > req.store_busy_threshold {
            return Ok(Err(task));
        }

        let region = coprocessor::RegionInfo {
            region_id: rpc_context.region.get_id(),
            region_epoch: Some(metapb::RegionEpoch {
                conf_ver: rpc_context.region.get_conf_ver(),
                version: rpc_context.region.get_ver(),
            }),
            ranges: task.ranges.to_pb_ranges(),
        };
        Ok(Ok(BatchedCopTask {
            task,
            region,
            store_id: rpc_context.store_id,
            peer: rpc_context.peer,
            load_based_replica_retry: replica_read != ReplicaReadLeader,
        }))
    }
}
