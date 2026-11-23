//! go-file: pkg/planner/core/plan_cache_instance.go
//!
//! The instance-level plan cache, shared by all sessions (the plan-cache exception A3, spec
//! §3.4). Go's lock-free linked lists under a `sync.Map` are replaced (A3 spec §1.2): the
//! entries are in a sharded hash map, each shard behind a `parking_lot::RwLock`; a lookup
//! holds the read lock only for the hash lookup, the parameter type check and one
//! `Arc::clone`. What Go does is kept: the plans of one key are tried newest first, `Put`
//! fails silently (eviction in progress, over the hard limit, a compatible plan already
//! there), and `Evict` computes the threshold of the least recently used entries with
//! Go's algorithm (`calcEvictionThreshold`, L1).
//!
//! Differences (all A3 decisions): the memory of an entry is the bytes it holds (decision 4);
//! `last_used` is written at most once per millisecond per entry (decision 7); there is a
//! `delete_all` for `ADMIN FLUSH INSTANCE PLAN_CACHE` (decision 8), with a flush generation
//! so that a statement that started optimizing before the flush does not put its plan
//! afterwards. Removed entries are dropped after the shard lock is released: the last
//! reference of a large plan tree is never freed under a lock.

use std::collections::HashMap;
use std::hash::BuildHasherDefault;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::time::Instant;

use crossbeam_utils::CachePadded;
use parking_lot::{Mutex, RwLock};
use smallvec::SmallVec;
use tidb_parser::types::FieldType;
use tidb_sessionctx::context::InstancePlanCache;

use crate::core::plan_cache_utils::{PlanCacheKey, PlanCacheValue, check_types_compatibility4_pc};

/// The map hashes the key's precomputed hash as it is (`PlanCacheKey` writes one `u64`).
// glue: A3 spec §3.4 (the key carries its seeded hash)
#[derive(Default)]
pub struct PassThroughHasher(u64);

impl std::hash::Hasher for PassThroughHasher {
    // glue: A3 spec §3.4
    fn finish(&self) -> u64 {
        self.0
    }
    // glue: A3 spec §3.4 (only write_u64 is used)
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = self.0.rotate_left(8) ^ u64::from(b);
        }
    }
    // glue: A3 spec §3.4
    fn write_u64(&mut self, v: u64) {
        self.0 = v;
    }
}

/// One cached plan (Go's `instancePCNode`, without the list link).
// glue: A3 spec §3.4 (instancePCNode)
struct Entry {
    value: Arc<PlanCacheValue>,
    /// Monotonic nanoseconds of the last use (Go's `lastUsed`, a `time.Time`).
    // interior: (b) shared state of the cache entry, written by every hit (throttled, decision 7)
    last_used: AtomicU64,
}

/// The plans of one key, newest first (Go's list after the head node).
type Plans = SmallVec<[Entry; 1]>;

type Shard = HashMap<PlanCacheKey, Plans, BuildHasherDefault<PassThroughHasher>>;

/// `last_used` is written only when it moves by more than this (A3 decision 7).
const LAST_USED_GRANULARITY_NANOS: u64 = 1_000_000;

/// instancePlanCache is the instance-level plan cache (see the module doc).
// glue: Go type instancePlanCache (a sharded map instead of lock-free lists, A3 spec §3.4)
// go-exception: EXC.A3 instancePlanCache.getHead (the sharded map has no head nodes, A3 spec §1.2)
// go-exception: EXC.A3 instancePlanCache.foreach (Evict walks the shards, A3 spec §1.2)
// go-exception: EXC.A3 instancePlanCache.headNodes (the sharded map has no head nodes, A3 spec §1.2)
// go-exception: EXC.A3 instancePlanCache.createNode (an entry is built in Put, A3 spec §1.2)
pub struct InstancePlanCacheImpl {
    shards: Box<[CachePadded<RwLock<Shard>>]>,
    shard_mask: u64,
    tot_cost: AtomicI64,
    tot_plan: AtomicI64,

    evict_mutex: Mutex<()>,
    in_evict: AtomicBool,
    soft_mem_limit: AtomicI64,
    hard_mem_limit: AtomicI64,
    /// Incremented by `delete_all` (A3 spec §3.4, the flush generation).
    flush_gen: AtomicU64,
    /// The origin of the monotonic `last_used` times.
    epoch: Instant,
}

// A3 spec §3.1 item 5: the cache is shared by every session.
const _: () = {
    // glue: compile-time check
    const fn send_sync<T: Send + Sync>() {}
    send_sync::<InstancePlanCacheImpl>();
    send_sync::<PlanCacheValue>();
};

/// The number of shards: a power of two, at least 64, four per CPU (A3 spec §3.4).
// glue: A3 spec §3.4
fn default_shard_count() -> usize {
    let cpus = std::thread::available_parallelism().map_or(8, |n| n.get());
    (cpus * 4).max(64).next_power_of_two()
}

/// NewInstancePlanCache creates a new instance level plan cache.
// go: NewInstancePlanCache L2 (the sharded map, A3 spec §3.4)
pub fn new_instance_plan_cache(soft_mem_limit: i64, hard_mem_limit: i64) -> InstancePlanCacheImpl {
    new_instance_plan_cache_with_shards(soft_mem_limit, hard_mem_limit, default_shard_count())
}

/// [`new_instance_plan_cache`] with `shards` shards (rounded up to a power of two).
// glue: A3 spec §3.4 (tests choose the shard count)
pub fn new_instance_plan_cache_with_shards(
    soft_mem_limit: i64,
    hard_mem_limit: i64,
    shards: usize,
) -> InstancePlanCacheImpl {
    let n = shards.max(1).next_power_of_two();
    InstancePlanCacheImpl {
        shards: (0..n)
            .map(|_| CachePadded::new(RwLock::new(Shard::default())))
            .collect(),
        shard_mask: (n - 1) as u64,
        tot_cost: AtomicI64::new(0),
        tot_plan: AtomicI64::new(0),
        evict_mutex: Mutex::new(()),
        in_evict: AtomicBool::new(false),
        soft_mem_limit: AtomicI64::new(soft_mem_limit),
        hard_mem_limit: AtomicI64::new(hard_mem_limit),
        flush_gen: AtomicU64::new(0),
        epoch: Instant::now(),
    }
}

/// The domain's factory (Go's `init` setting `domain.NewInstancePlanCache`).
// go: init L2 (the factory is registered by the caller that starts the domain, see tidb_domain)
pub fn register_instance_plan_cache_factory() {
    tidb_domain::set_new_instance_plan_cache(|soft, hard| {
        Arc::new(new_instance_plan_cache(soft, hard)) as Arc<dyn InstancePlanCache>
    });
}

/// Resets `in_evict` when the eviction ends (Go's `defer pc.inEvict.Store(false)`).
struct InEvict<'a>(&'a AtomicBool);

impl Drop for InEvict<'_> {
    // glue: Go defer
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

impl InstancePlanCacheImpl {
    /// Monotonic nanoseconds since the cache was built, at least 1 (0 is Go's zero time).
    // glue: time.Now() of lastUsed (monotonic)
    fn now(&self) -> u64 {
        self.epoch.elapsed().as_nanos() as u64 + 1
    }

    /// The shard of a key: bits 32.. of its hash (the map inside the shard uses the low bits
    /// for the bucket and the top 7 for its control bytes).
    // glue: A3 spec §3.4 (shard selection)
    fn shard(&self, key: &PlanCacheKey) -> &RwLock<Shard> {
        &self.shards[((key.hash_value() >> 32) & self.shard_mask) as usize]
    }

    /// The flush generation: a statement reads it before optimizing and passes it to
    /// [`InstancePlanCacheImpl::put`] (A3 spec §3.4).
    // glue: A3 spec §3.4 (flush generation)
    pub fn flush_generation(&self) -> u64 {
        self.flush_gen.load(Ordering::SeqCst)
    }

    /// Get gets the cached value according to key and paramTypes.
    // go: instancePlanCache.Get L1
    pub fn get(
        &self,
        key: &PlanCacheKey,
        param_types: &[FieldType],
    ) -> Option<Arc<PlanCacheValue>> {
        let shard = self.shard(key).read();
        let plans = shard.get(key)?; // cache miss
        self.get_plan_from_list(plans, param_types)
    }

    // go: instancePlanCache.getPlanFromList L2 (the last_used write is throttled, A3 decision 7)
    fn get_plan_from_list(
        &self,
        plans: &Plans,
        param_types: &[FieldType],
    ) -> Option<Arc<PlanCacheValue>> {
        for node in plans {
            if check_types_compatibility4_pc(&node.value.param_types, param_types) {
                // v.Plan is read-only, no need to lock
                if !self.in_evict.load(Ordering::SeqCst) {
                    let now = self.now();
                    if now > node.last_used.load(Ordering::Relaxed) + LAST_USED_GRANULARITY_NANOS {
                        node.last_used.store(now, Ordering::Relaxed); // atomically update the lastUsed field
                    }
                }
                return Some(Arc::clone(&node.value));
            }
        }
        None
    }

    /// Put puts the key and values into the cache. `flush_gen` is the generation read when
    /// the statement started optimizing ([`InstancePlanCacheImpl::flush_generation`]).
    /// Due to some thread-safety issues, this Put operation might fail, use the returned succ to indicate it.
    // go: instancePlanCache.Put L2 (the compatibility check and the insert under one write lock; the flush generation, A3 spec §3.4)
    pub fn put(
        &self,
        key: &PlanCacheKey,
        value: Arc<PlanCacheValue>,
        param_types: &[FieldType],
        flush_gen: u64,
    ) -> bool {
        if self.in_evict.load(Ordering::SeqCst) {
            return false; // do nothing if eviction is in progress
        }
        let v_mem = value.memory_usage();
        if v_mem + self.tot_cost.load(Ordering::SeqCst) > self.hard_mem_limit.load(Ordering::SeqCst)
        {
            return false; // do nothing if it exceeds the hard limit
        }
        let mut shard = self.shard(key).write();
        if self.in_evict.load(Ordering::SeqCst) {
            return false; // do nothing if eviction is in progress
        }
        if self.flush_gen.load(Ordering::SeqCst) != flush_gen {
            return false; // the cache was flushed after this statement started optimizing
        }
        let plans = shard.entry(key.clone()).or_default();
        if plans
            .iter()
            .any(|n| check_types_compatibility4_pc(&n.value.param_types, param_types))
        {
            return false; // some other thread has inserted the same plan before
        }
        plans.insert(
            0,
            Entry {
                value,
                last_used: AtomicU64::new(self.now()),
            },
        );
        self.tot_cost.fetch_add(v_mem, Ordering::SeqCst);
        self.tot_plan.fetch_add(1, Ordering::SeqCst);
        true
    }

    /// Every cached value, one shard at a time (for tests and diagnostics).
    // glue: A3 spec §5.2 (the tests read the cached plans)
    pub fn values(&self) -> Vec<Arc<PlanCacheValue>> {
        let mut v = Vec::new();
        for shard in self.shards.iter() {
            for plans in shard.read().values() {
                v.extend(plans.iter().map(|e| Arc::clone(&e.value)));
            }
        }
        v
    }

    /// The `last_used` of every entry (the first pass of `Evict`), one shard at a time.
    // go-part: instancePlanCache.Evict (step 1)
    fn last_used_times(&self) -> Vec<u64> {
        let mut times = Vec::with_capacity(64);
        for shard in self.shards.iter() {
            let shard = shard.read();
            for plans in shard.values() {
                times.extend(plans.iter().map(|n| n.last_used.load(Ordering::Relaxed)));
            }
        }
        times
    }

    /// Removes the entries `remove` selects, one shard at a time; the removed values are
    /// returned to be dropped by the caller after every lock is released.
    // glue: A3 spec §3.4 (the removal of Evict / delete_all)
    fn remove_where(&self, remove: impl Fn(&Entry) -> bool) -> Vec<Arc<PlanCacheValue>> {
        let mut removed = Vec::new();
        for shard in self.shards.iter() {
            let mut shard = shard.write();
            shard.retain(|_, plans| {
                let mut i = 0;
                while i < plans.len() {
                    if remove(&plans[i]) {
                        let e = plans.remove(i);
                        self.tot_cost
                            .fetch_sub(e.value.memory_usage(), Ordering::SeqCst);
                        self.tot_plan.fetch_sub(1, Ordering::SeqCst);
                        removed.push(e.value);
                    } else {
                        i += 1;
                    }
                }
                // post operation: clear empty heads
                !plans.is_empty()
            });
        }
        removed
    }

    /// calcEvictionThreshold: the last-used time at or before which entries are evicted
    /// (0, Go's zero time, evicts nothing).
    // go: instancePlanCache.calcEvictionThreshold L1
    fn calc_eviction_threshold(&self, last_used_times: &mut [u64]) -> u64 {
        calc_eviction_threshold(
            last_used_times,
            self.tot_cost.load(Ordering::SeqCst),
            self.soft_mem_limit.load(Ordering::SeqCst),
        )
    }
}

/// calcEvictionThreshold over the given totals (the method reads them from the cache).
// go-part: instancePlanCache.calcEvictionThreshold
pub fn calc_eviction_threshold(
    last_used_times: &mut [u64],
    tot_cost: i64,
    soft_mem_limit: i64,
) -> u64 {
    if last_used_times.is_empty() {
        return 0;
    }
    let avg_per_plan = tot_cost / last_used_times.len() as i64;
    if avg_per_plan <= 0 {
        return 0;
    }
    let mem_to_release = tot_cost - soft_mem_limit;
    // (... +avgPerPlan-1) is used to try to keep the final memory usage below the soft mem limit.
    let num_to_evict = (mem_to_release + avg_per_plan - 1) / avg_per_plan;
    if num_to_evict <= 0 {
        return 0;
    }
    last_used_times.sort_unstable();
    if (last_used_times.len() as i64) < num_to_evict {
        return 0; // for safety, avoid index-of-range panic
    }
    last_used_times[(num_to_evict - 1) as usize]
}

impl InstancePlanCache for InstancePlanCacheImpl {
    /// Evict evicts some values. There should be a background thread to perform the eviction.
    /// step 1: iterate all values to collect their last_used
    /// step 2: estimate an eviction threshold time based on all last_used values
    /// step 3: iterate all values again and evict qualified values
    // go: instancePlanCache.Evict L2 (shard by shard; the evicted plans are dropped outside the locks)
    fn evict(&self) -> (String, usize) {
        let _guard = self.evict_mutex.lock(); // make sure only one thread to trigger eviction for safety
        self.in_evict.store(true, Ordering::SeqCst);
        let _reset = InEvict(&self.in_evict);
        let (current_tot, soft_limit) = (
            self.tot_cost.load(Ordering::SeqCst),
            self.soft_mem_limit.load(Ordering::SeqCst),
        );
        if current_tot < soft_limit {
            return (
                format!(
                    "memory usage is below the soft limit, currentTot: {current_tot}, softLimit: {soft_limit}"
                ),
                0,
            );
        }
        let mut last_used_times = self.last_used_times(); // step 1
        let threshold = self.calc_eviction_threshold(&mut last_used_times); // step 2
        let detail_info = format!("evict threshold: {threshold}");
        // step 3: if lastUsed<=threshold, evict this value
        let removed = self.remove_where(|e| e.last_used.load(Ordering::Relaxed) <= threshold);
        let num_evicted = removed.len();
        drop(_reset);
        drop(_guard);
        drop(removed);
        (detail_info, num_evicted)
    }

    /// Size returns the number of plans in this plan cache.
    // go: instancePlanCache.Size L1
    fn size(&self) -> i64 {
        self.tot_plan.load(Ordering::SeqCst)
    }

    /// MemUsage returns the memory usage of this plan cache.
    // go: instancePlanCache.MemUsage L1
    fn mem_usage(&self) -> i64 {
        self.tot_cost.load(Ordering::SeqCst)
    }

    /// GetLimits gets the memory limit of this plan cache.
    // go: instancePlanCache.GetLimits L1
    fn get_limits(&self) -> (i64, i64) {
        (
            self.soft_mem_limit.load(Ordering::SeqCst),
            self.hard_mem_limit.load(Ordering::SeqCst),
        )
    }

    /// SetLimits sets the memory limit of this plan cache.
    // go: instancePlanCache.SetLimits L1
    fn set_limits(&self, soft_limit: i64, hard_limit: i64) {
        self.soft_mem_limit.store(soft_limit, Ordering::SeqCst);
        self.hard_mem_limit.store(hard_limit, Ordering::SeqCst);
    }

    /// Removes every plan (`ADMIN FLUSH INSTANCE PLAN_CACHE`, A3 decision 8): bumps the flush
    /// generation first, so that no plan optimized before the flush is put afterwards.
    // glue: A3 decision 8 (delete_all)
    fn delete_all(&self) -> usize {
        let guard = self.evict_mutex.lock();
        self.flush_gen.fetch_add(1, Ordering::SeqCst);
        let removed = self.remove_where(|_| true);
        let n = removed.len();
        drop(guard);
        drop(removed);
        n
    }

    // glue: Go's type assertion of the any-typed Get / Put
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
