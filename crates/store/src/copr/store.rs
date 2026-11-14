//! go-file: pkg/store/copr/store.go
//!
//! `Store` wraps the TiKV store (the [`TiKVStore`] boundary, ADR 0008) and provides the
//! coprocessor client.

// kv.TiKV / TiFlash / TiDB keep their Go names (M-GONAME).
#![allow(non_upper_case_globals)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use tidb_base::errors::Error;
use tidb_base::kv::{StoreType, TiDB, TiFlash, TiKV};
use tidb_runtime::pool::ExecPool;

use super::coprocessor::CopClient;
use super::region_cache::{RegionCache, new_region_cache};
use super::tikv::{EndpointType, TiKVStore};

/// Store wraps tikv.KVStore and provides coprocessor utilities.
///
/// `coprCache` is the coprocessor cache of D07.4 (exception A3), not built: always `None`.
/// `pool` is the N3 ExecPool the iterator's workers run on (M-GO); `None` runs them on the
/// current tokio runtime (tests).
pub struct Store<S: TiKVStore> {
    /// `kvStore.store`.
    pub kv_store: Arc<S>,
    pub(crate) copr_cache: Option<()>,
    replica_read_seed: AtomicU32,
    pub(crate) numcpu: isize,
    pub(crate) pool: Option<ExecPool>,
}

/// NewStore creates a new store instance. The coprocessor cache config is D07.4's.
// go: NewStore L1
pub fn new_store<S: TiKVStore>(s: Arc<S>, pool: Option<ExecPool>, seed: u32) -> Store<S> {
    Store {
        kv_store: s,
        copr_cache: None,
        replica_read_seed: AtomicU32::new(seed),
        numcpu: std::thread::available_parallelism().map_or(1, |n| n.get()) as isize,
        pool,
    }
}

impl<S: TiKVStore> Store<S> {
    /// Close releases resources allocated for coprocessor.
    // go: Store.Close L1
    pub fn close(&self) {
        // Only the coprocessor cache (D07.4) holds resources.
    }

    // go: Store.nextReplicaReadSeed L1
    fn next_replica_read_seed(&self) -> u32 {
        self.replica_read_seed
            .fetch_add(1, Ordering::SeqCst)
            .wrapping_add(1)
    }

    /// GetClient gets a client instance.
    // go: Store.GetClient L1
    pub fn get_client(self: &Arc<Self>) -> CopClient<S> {
        CopClient {
            store: self.clone(),
            replica_read_seed: self.next_replica_read_seed(),
        }
    }

    /// GetRegionCache returns the region cache instance.
    // go: kvStore.GetRegionCache L1
    pub fn get_region_cache(&self) -> RegionCache<S> {
        new_region_cache(self.kv_store.clone())
    }

    /// CheckVisibility checks if it is safe to read using given ts.
    // go: kvStore.CheckVisibility L1
    pub fn check_visibility(&self, start_time: u64) -> Result<(), Error> {
        self.kv_store.check_visibility(start_time)
    }
}

/// `DisaggregatedTiFlash` is not supported (TiFlash is DEF.MPP): TiFlash maps to `TiFlash`.
// go: getEndPointType L1
pub(crate) fn get_end_point_type(t: StoreType) -> EndpointType {
    match t {
        TiKV => EndpointType::TiKV,
        TiFlash => EndpointType::TiFlash,
        TiDB => EndpointType::TiDB,
        _ => EndpointType::TiKV,
    }
}
