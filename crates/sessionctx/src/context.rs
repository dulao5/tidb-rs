//! go-file: pkg/sessionctx/context.go
//!
//! Block D00.1 holds two functions of this file: `basicCtxType.String` and
//! `ValidateSnapshotReadTS`. The `sessionctx.Context` interface itself is the session (N2 §3.1:
//! `Session` owning `SessionVars`, `Arc<StmtCtx>` and the shared parts) and comes with D05.3.
//! `ValidateSnapshotReadTS` needs `kv.Storage.GetOracle` (the oracle client, D07.1) and is
//! exempted until then.

// Constants keep their Go names (mapping rule M-GONAME).
#![allow(non_upper_case_globals)]

/// The keys of the session context values (Go `basicCtxType`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BasicCtxType(pub i64);

/// QueryString is the key for original query string.
pub const QueryString: BasicCtxType = BasicCtxType(1);
/// Initing is the key for indicating if the server is running bootstrap or upgrade job.
pub const Initing: BasicCtxType = BasicCtxType(2);
/// LastExecuteDDL is the key for whether the session execute a ddl command last time.
pub const LastExecuteDDL: BasicCtxType = BasicCtxType(3);

impl BasicCtxType {
    /// String implements fmt.Stringer.
    // go: basicCtxType.String L1
    pub fn string(self) -> &'static str {
        match self {
            QueryString => "query_string",
            Initing => "initing",
            LastExecuteDDL => "last_execute_ddl",
            _ => "unknown",
        }
    }
}

impl std::fmt::Display for BasicCtxType {
    // glue: fmt.Stringer, used by `%s` / `%v`
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.string())
    }
}

/// InstancePlanCache represents the instance/node level plan cache.
///
/// Go types `Get` / `Put` with `any` (the value is the planner's `*PlanCacheValue`, the
/// options its parameter types) to avoid an import cycle. Here the trait carries the
/// management part the domain and the session call (eviction, limits, size, the flush of
/// `ADMIN FLUSH INSTANCE PLAN_CACHE`); the planner's implementation has the typed `get` /
/// `put` and reaches them through [`InstancePlanCache::as_any`] (the plan-cache exception
/// A3 spec §3.8: the interface here, the implementation in the planner, injected at start).
// glue: interface sessionctx.InstancePlanCache (Get / Put are typed methods of the planner's implementation)
pub trait InstancePlanCache: Send + Sync {
    /// Evict evicts some cached values.
    // glue: interface method sessionctx.InstancePlanCache.Evict
    fn evict(&self) -> (String, usize);
    /// Size returns the number of cached values.
    // glue: interface method sessionctx.InstancePlanCache.Size
    fn size(&self) -> i64;
    /// MemUsage returns the total memory usage of this plan cache.
    // glue: interface method sessionctx.InstancePlanCache.MemUsage
    fn mem_usage(&self) -> i64;
    /// GetLimits returns the soft and hard memory limits of this plan cache.
    // glue: interface method sessionctx.InstancePlanCache.GetLimits
    fn get_limits(&self) -> (i64, i64);
    /// SetLimits sets the soft and hard memory limits of this plan cache.
    // glue: interface method sessionctx.InstancePlanCache.SetLimits
    fn set_limits(&self, soft_limit: i64, hard_limit: i64);
    /// Removes every cached plan (`ADMIN FLUSH INSTANCE PLAN_CACHE`; Go's instance cache has
    /// no such operation, the plan-cache exception A3 decision 8). Returns the number removed.
    // glue: A3 decision 8 (delete_all of the instance plan cache)
    fn delete_all(&self) -> usize;
    /// The implementation, for the planner's typed `get` / `put`.
    // glue: Go's type assertion of the any-typed Get / Put
    fn as_any(&self) -> &dyn std::any::Any;
}

/// The instance plan caches whose limits follow the variables (the domain registers its
/// cache when it builds it).
static INSTANCE_PLAN_CACHES: parking_lot::Mutex<Vec<std::sync::Weak<dyn InstancePlanCache>>> =
    parking_lot::Mutex::new(Vec::new());

/// The soft and hard memory limits of the instance plan cache from
/// `tidb_instance_plan_cache_max_size` and `tidb_instance_plan_cache_reserved_percentage`
/// (Go computes them in `InitInstancePlanCache` and `planCacheMetricsAndVars`).
// go-part: Domain.InitInstancePlanCache (the limits)
pub fn instance_plan_cache_limits() -> (i64, i64) {
    use crate::variable::tidb_vars::{
        InstancePlanCacheMaxMemSize, InstancePlanCacheReservedPercentage,
    };
    let hard_limit = InstancePlanCacheMaxMemSize.load(std::sync::atomic::Ordering::SeqCst);
    let soft_limit =
        (hard_limit as f64 * (1.0 - InstancePlanCacheReservedPercentage.load())) as i64;
    (soft_limit, hard_limit)
}

/// Makes `cache` follow the limit variables right after a `SET GLOBAL` (the plan-cache
/// exception A3 decision 5; Go applies them within 15 s).
// glue: A3 decision 5
pub fn register_instance_plan_cache(cache: &std::sync::Arc<dyn InstancePlanCache>) {
    let mut caches = INSTANCE_PLAN_CACHES.lock();
    caches.retain(|c| c.strong_count() > 0);
    caches.push(std::sync::Arc::downgrade(cache));
}

/// Applies the limit variables to every registered instance plan cache whose limits
/// differ (the `SetGlobal` hooks of the two limit variables, A3 decision 5).
// go-part: Domain.planCacheMetricsAndVars (update limits), run by SET GLOBAL (A3 decision 5)
pub fn apply_instance_plan_cache_limits() {
    let (soft_limit, hard_limit) = instance_plan_cache_limits();
    let caches: Vec<_> = INSTANCE_PLAN_CACHES
        .lock()
        .iter()
        .filter_map(std::sync::Weak::upgrade)
        .collect();
    for cache in caches {
        let (cur_soft, cur_hard) = cache.get_limits();
        if cur_soft != soft_limit || cur_hard != hard_limit {
            cache.set_limits(soft_limit, hard_limit);
        }
    }
}
