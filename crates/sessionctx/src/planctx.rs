//! go-file: pkg/planner/planctx/context.go
//!
//! Mapping:
//! - `PlanContext` is a trait (object-safe, `Send + Sync`). Its methods whose types are ported
//!   are declared; the ones over unported types (`GetSessionVars` — under N2 the planner gets
//!   `&mut PlanCtx<'_>` holding `&mut SessionVars`, D05.3; `GetSQLExecutor` /
//!   `GetRestrictedSQLExecutor` (util/sqlexec); `GetDomainInfoSchema` / `GetInfoSchema`
//!   (D06.3); `GetMPPClient` (DEF.MPP); `GetSessionManager`; `Txn`; the value-store and
//!   table-lock contexts) are added with the blocks that port those types.
//! - `map[string]struct{}` is a `HashSet<String>` (Go's nil map reads like an empty set).
//! - `BuildPBContext` is a plain struct; Go's struct copy in `Detach` is `Clone` (the shared
//!   interface values are `Arc`s, as Go copies the interface pointers).

use std::collections::HashSet;
use std::sync::Arc;

use tidb_base::Error;
use tidb_base::contextutil::WarnAppenderRef;
use tidb_kv::kv::{Client, Storage};
use tidb_model::TableItemID;

use crate::exprctx::{BuildContextRef, ExprContextRef};
use crate::ranger::context::RangerContext;

/// PlanContext is the context for building plan.
pub trait PlanContext: Send + Sync {
    /// GetExprCtx gets the expression context.
    // glue: interface method of planctx.PlanContext
    fn get_expr_ctx(&self) -> ExprContextRef;
    /// GetNullRejectCheckExprCtx gets the expression context with null rejected check.
    // glue: interface method of planctx.PlanContext
    fn get_null_reject_check_expr_ctx(&self) -> ExprContextRef;
    /// GetStore returns the store of session.
    // glue: interface method of planctx.PlanContext
    fn get_store(&self) -> Arc<dyn Storage>;
    /// UpdateColStatsUsage updates the column stats usage.
    // glue: interface method of planctx.PlanContext (iter.Seq is a slice)
    fn update_col_stats_usage(&self, predicate_columns: &[TableItemID]);
    /// GetClient gets a kv.Client.
    // glue: interface method of planctx.PlanContext
    fn get_client(&self) -> Arc<dyn Client>;
    /// HasDirtyContent checks whether there's dirty update on the given table.
    // glue: interface method of planctx.PlanContext
    fn has_dirty_content(&self, tid: i64) -> bool;
    /// AdviseTxnWarmup advises the txn to warm up.
    // glue: interface method of planctx.PlanContext
    fn advise_txn_warmup(&self) -> Result<(), Error>;
    /// GetRangerCtx returns the context used in `ranger` functions
    // glue: interface method of planctx.PlanContext
    fn get_ranger_ctx(&self) -> Arc<RangerContext>;
    /// GetBuildPBCtx returns the context used in `ToPB` method.
    // glue: interface method of planctx.PlanContext
    fn get_build_pb_ctx(&self) -> Arc<BuildPBContext>;
    /// SetReadonlyUserVarMap sets the readonly user variable map.
    // glue: interface method of planctx.PlanContext
    fn set_readonly_user_var_map(&self, readonly_user_vars: HashSet<String>);
    /// GetReadonlyUserVarMap gets the readonly user variable map.
    // glue: interface method of planctx.PlanContext
    fn get_readonly_user_var_map(&self) -> HashSet<String>;
    /// Reset reset the local context.
    // glue: interface method of planctx.PlanContext
    fn reset(&self);
    /// BuiltinFunctionUsageInc increase the counting of each builtin function usage
    /// Notice that this is a thread safe function
    // glue: interface method of planctx.PlanContext
    fn builtin_function_usage_inc(&self, scalar_func_sig_name: &str);
}

/// EmptyPlanContextExtended is used to provide some empty implementations for PlanContext.
/// It is used by some mock contexts that are only required to implement PlanContext
/// but do not care about the actual implementation.
#[derive(Clone, Copy, Debug, Default)]
pub struct EmptyPlanContextExtended;

impl EmptyPlanContextExtended {
    /// AdviseTxnWarmup advises the txn to warm up.
    // go: EmptyPlanContextExtended.AdviseTxnWarmup L1
    pub fn advise_txn_warmup(&self) -> Result<(), Error> {
        Ok(())
    }

    /// SetReadonlyUserVarMap sets the readonly user variable map.
    // go: EmptyPlanContextExtended.SetReadonlyUserVarMap L1
    pub fn set_readonly_user_var_map(&self, _readonly_user_vars: HashSet<String>) {}

    /// GetReadonlyUserVarMap gets the readonly user variable map.
    // go: EmptyPlanContextExtended.GetReadonlyUserVarMap L1
    pub fn get_readonly_user_var_map(&self) -> HashSet<String> {
        HashSet::new()
    }

    /// Reset implements the PlanContext interface.
    // go: EmptyPlanContextExtended.Reset L1
    pub fn reset(&self) {}
}

/// BuildPBContext is used to build the `*tipb.Executor` according to the plan.
#[derive(Clone)]
pub struct BuildPBContext {
    /// ExprCtx is the expression build context.
    pub expr_ctx: BuildContextRef,
    /// Client is the kv client.
    pub client: Arc<dyn Client>,

    /// TiFlashFastScan is the value of `tiflash_fastscan`.
    pub tiflash_fast_scan: bool,
    /// TiFlashFineGrainedShuffleBatchSize is the value of
    /// `tiflash_fine_grained_shuffle_batch_size`.
    pub tiflash_fine_grained_shuffle_batch_size: u64,

    // the following fields are used to build `expression.PushDownContext`.
    // TODO: it'd be better to embed `expression.PushDownContext` in `BuildPBContext`. But `expression` already
    // depends on this package, so we need to move `expression.PushDownContext` to a standalone package first.
    /// GroupConcatMaxLen is the value of `group_concat_max_len`.
    pub group_concat_max_len: u64,
    /// InExplainStmt indicates the statement is EXPLAIN.
    pub in_explain_stmt: bool,
    /// WarnHandler receives the warnings.
    pub warn_handler: Option<WarnAppenderRef>,
    /// ExtraWarnghandler receives the extra warnings (Go's spelling of the field).
    pub extra_warnghandler: Option<WarnAppenderRef>,
}

impl BuildPBContext {
    /// GetExprCtx returns the expression context.
    // go: BuildPBContext.GetExprCtx L1
    pub fn get_expr_ctx(&self) -> BuildContextRef {
        Arc::clone(&self.expr_ctx)
    }

    /// GetClient returns the kv client.
    // go: BuildPBContext.GetClient L1
    pub fn get_client(&self) -> Arc<dyn Client> {
        Arc::clone(&self.client)
    }

    /// Detach detaches this context from the session context.
    ///
    /// NOTE: Though this session context can be used parallelly with this context after calling
    /// it, the `StatementContext` cannot. The session context should create a new `StatementContext`
    /// before executing another statement.
    // go: BuildPBContext.Detach L1
    pub fn detach(&self, static_expr_ctx: BuildContextRef) -> BuildPBContext {
        let mut new_ctx = self.clone();
        new_ctx.expr_ctx = static_expr_ctx;
        new_ctx
    }
}
