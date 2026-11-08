//! go-file: pkg/util/ranger/context/context.go
//!
//! `RangerContext`, the context used to build ranges (block D00.1, N0 review: the ranger
//! types live with the context interfaces). Go embeds `*contextutil.RangeFallbackHandler`
//! and `*contextutil.PlanCacheTracker`; here they are named shared fields.

use std::collections::HashMap;
use std::sync::Arc;

use tidb_base::contextutil::PlanCacheTracker;
use tidb_base::contextutil::RangeFallbackHandler;

use crate::errctx;
use crate::exprctx::BuildContextRef;

/// RangerContext is the context used to build range.
#[derive(Clone)]
pub struct RangerContext {
    pub type_ctx: tidb_types::context::Context,
    pub err_ctx: errctx::Context,
    pub expr_ctx: BuildContextRef,
    pub range_fallback_handler: Arc<RangeFallbackHandler>,
    pub plan_cache_tracker: Arc<PlanCacheTracker>,
    pub optimizer_fix_control: HashMap<u64, String>,
    pub use_cache: bool,
    pub in_prepared_plan_building: bool,
    pub regard_null_as_point: bool,
    pub opt_prefix_index_single_scan: bool,
}

impl RangerContext {
    /// Detach detaches this context from the session context.
    ///
    /// NOTE: Though this session context can be used parallelly with this context after
    /// calling it, the `StatementContext` cannot. The session context should create a new
    /// `StatementContext` before executing another statement.
    // go: RangerContext.Detach L1
    pub fn detach(&self, static_expr_ctx: BuildContextRef) -> RangerContext {
        let mut new_ctx = self.clone();
        new_ctx.expr_ctx = static_expr_ctx;
        let mut m = HashMap::with_capacity(self.optimizer_fix_control.len());
        for (k, v) in &self.optimizer_fix_control {
            m.insert(*k, v.clone());
        }
        new_ctx.optimizer_fix_control = m;
        new_ctx
    }
}
