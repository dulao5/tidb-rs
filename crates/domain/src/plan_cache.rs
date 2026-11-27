//! go-file: pkg/domain/domain.go
//!
//! The instance plan cache of the domain (the plan-cache exception A3, spec §3.8): Go's
//! `InitInstancePlanCache`, `GetInstancePlanCache`, `planCacheMetricsAndVars` and
//! `planCacheEvictTrigger`.
//!
//! Mapping:
//! - The cache is the planner's (above this crate); Go's package variable
//!   `NewInstancePlanCache`, set by the planner's `init`, is [`set_new_instance_plan_cache`],
//!   called by whoever starts the domain before [`Domain::init_instance_plan_cache`].
//! - The two background loops run on the tokio runtime of the caller (N3 §3.7); without a
//!   runtime (tests) they are not started and the caller drives eviction itself.
//! - The limits are also applied right away by `SET GLOBAL` (A3 decision 5: Go takes up to
//!   15 s): the cache is registered with `tidb_sessionctx::context`, whose
//!   `apply_instance_plan_cache_limits` the variables' `SetGlobal` hooks call; the 15 s loop
//!   stays as in Go.
//! - The eviction period is Go's fixed 30 s (A3 decision 6: no variable).

use std::sync::{Arc, OnceLock, Weak};
use std::time::{Duration, Instant};

use tidb_sessionctx::context::InstancePlanCache;

use crate::Domain;

/// The constructor of the instance plan cache (`softMemLimit`, `hardMemLimit`).
pub type NewInstancePlanCacheFn = fn(i64, i64) -> Arc<dyn InstancePlanCache>;

/// `NewInstancePlanCache`: set by the planner (Go's `init` of `planner/core`).
static NEW_INSTANCE_PLAN_CACHE: OnceLock<NewInstancePlanCacheFn> = OnceLock::new();

/// Sets `domain.NewInstancePlanCache` (idempotent: the first setter wins, as there is one
/// implementation).
// glue: Go's package variable domain.NewInstancePlanCache, set by planner/core init
pub fn set_new_instance_plan_cache(f: NewInstancePlanCacheFn) {
    let _ = NEW_INSTANCE_PLAN_CACHE.set(f);
}

pub use tidb_sessionctx::context::{apply_instance_plan_cache_limits, instance_plan_cache_limits};

/// Applies the limit variables to `cache` if they changed (the first half of
/// `planCacheMetricsAndVars`'s tick).
// go-part: Domain.planCacheMetricsAndVars (update limits)
fn update_limits(cache: &dyn InstancePlanCache) {
    let (soft_limit, hard_limit) = instance_plan_cache_limits();
    let (cur_soft, cur_hard) = cache.get_limits();
    if cur_soft != soft_limit || cur_hard != hard_limit {
        cache.set_limits(soft_limit, hard_limit);
    }
}

impl Domain {
    /// InitInstancePlanCache initializes the instance level plan cache for this Domain.
    /// Idempotent; a no-op when no constructor is registered. The background loops start on
    /// the current tokio runtime, if any.
    // go: Domain.InitInstancePlanCache L2 (idempotent; loops on the caller's runtime, see the module doc)
    pub fn init_instance_plan_cache(self: &Arc<Self>) {
        let Some(new) = NEW_INSTANCE_PLAN_CACHE.get() else {
            return;
        };
        let mut started = false;
        self.instance_plan_cache.get_or_init(|| {
            started = true;
            let (soft_limit, hard_limit) = instance_plan_cache_limits();
            let cache = new(soft_limit, hard_limit);
            tidb_sessionctx::context::register_instance_plan_cache(&cache);
            cache
        });
        if !started {
            return;
        }
        // use a separate goroutine to avoid the eviction blocking other operations.
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(plan_cache_evict_trigger(Arc::downgrade(self)));
            rt.spawn(plan_cache_metrics_and_vars(Arc::downgrade(self)));
        }
    }

    /// GetInstancePlanCache returns the instance level plan cache in this Domain (`None`
    /// before [`Domain::init_instance_plan_cache`]).
    // go: Domain.GetInstancePlanCache L1
    pub fn get_instance_plan_cache(&self) -> Option<Arc<dyn InstancePlanCache>> {
        self.instance_plan_cache.get().cloned()
    }
}

/// planCacheMetricsAndVars updates metrics and variables for Instance Plan Cache
/// periodically. Ends when the domain is dropped (Go's `do.exit`).
// go: Domain.planCacheMetricsAndVars L2 (ends with the domain)
async fn plan_cache_metrics_and_vars(domain: Weak<Domain>) {
    let mut ticker = tokio::time::interval(Duration::from_secs(15)); // 15s by default
    ticker.tick().await;
    loop {
        ticker.tick().await;
        let Some(cache) = domain.upgrade().and_then(|d| d.get_instance_plan_cache()) else {
            tidb_base::log_info!("planCacheMetricsAndVars exited.");
            return;
        };
        // update limits
        update_limits(cache.as_ref());
        // update the metrics
        tidb_metrics::plan_cache_instance_num(cache.size());
        tidb_metrics::plan_cache_instance_memory_usage(cache.mem_usage());
    }
}

/// planCacheEvictTrigger triggers the plan cache eviction periodically. Ends when the
/// domain is dropped (Go's `do.exit`).
// go: Domain.planCacheEvictTrigger L2 (ends with the domain)
async fn plan_cache_evict_trigger(domain: Weak<Domain>) {
    let mut ticker = tokio::time::interval(Duration::from_secs(30)); // 30s by default
    ticker.tick().await;
    loop {
        ticker.tick().await;
        let Some(cache) = domain.upgrade().and_then(|d| d.get_instance_plan_cache()) else {
            tidb_base::log_info!("planCacheEvictTrigger exited.");
            return;
        };
        // trigger the eviction: on the blocking pool, it walks every shard (N3 §3.7)
        let begin = Instant::now();
        let c = Arc::clone(&cache);
        let Ok((detail_info, num_evicted)) = tokio::task::spawn_blocking(move || c.evict()).await
        else {
            continue;
        };
        tidb_metrics::plan_cache_instance_last_evict(num_evicted as i64);
        if num_evicted > 0 {
            tidb_base::log_info!(
                "instance plan eviction",
                "detail" => &detail_info,
                "num_evicted" => num_evicted as i64,
                "time_spent" => begin.elapsed(),
            );
        }
    }
}
