//! glue: what the executor framework (D12.0, VS1-13) needs from blocks above it or not ported
//! yet, as narrow interfaces and stand-in types (constitution §2.4).
//!
//! - [`SessionContext`]: the `sessionctx.Context` methods (and the `sessiontxn` transaction
//!   manager calls) that `ExecStmt`, `recordSet`, the compiler and the executor builder make.
//!   The session (D05.3, rank 15, above this crate) implements it in VS1-14, as it implements
//!   `tidb_sessiontxn::sctx::SessionCtx` (#219).
//! - [`ResultField`]: `resolve.ResultField` (`pkg/planner/core/resolve/result.go`, D09.0,
//!   VS1-10); moves to the planner when D09.0 lands.
//! - [`PlanCacheStmt`]: `plannercore.PlanCacheStmt` (D13.1) of the planner; `ExecStmt.PointGet`
//!   reads its cached point get executor.
//! - [`Plan`]: Go's `base.Plan`. The new stack only builds physical plans in VS1 (no
//!   `Execute`, `Set`, `Simple`, ... plans), so it is the physical plan.

use std::sync::Arc;
use std::time::SystemTime;

use futures::future::BoxFuture;
use tidb_base::Error;
use tidb_kv::kv::Snapshot;
use tidb_memory::sqlkiller::SqlKiller;
use tidb_parser::model::CIStr;
use tidb_parser::types::FieldType;
use tidb_sessionctx::distsql_ctx::DistSQLContext;
use tidb_sessionctx::exprctx::ExprContextRef;
use tidb_sessionctx::planctx::BuildPBContext;
use tidb_sessionctx::ranger::context::RangerContext;
use tidb_sessionctx::variable::session::SessionVars;
use tidb_sessionctx::variable::session_state::PlanReplayerTaskKey;

use crate::internal::exec::executor::Executor;

/// `infoschema.InfoSchema` (the new stack only knows v2, VS1 plan §4.1).
pub type InfoSchemaRef = Arc<tidb_infoschema::InfoSchemaV2>;

/// Go's `base.Plan` (see the module comment).
pub type Plan = tidb_planner::PhysicalPlanRef;

/// `*domain.planReplayerHandle` as far as the capture check reads it: the capture tasks its
/// collector loaded from `mysql.plan_replayer_task`.
pub trait PlanReplayerHandle: Send + Sync {
    /// `GetTasks()`.
    // glue: method planReplayerTaskCollectorHandle.GetTasks
    fn get_tasks(&self) -> Vec<PlanReplayerTaskKey>;
}

/// The `sessionctx.Context` surface of the executor framework (see the module comment).
pub trait SessionContext: Send {
    /// `GetSessionVars()`.
    // glue: interface method sessionctx.Context.GetSessionVars
    fn get_session_vars(&self) -> &SessionVars;
    /// `GetSessionVars()`, for writing.
    // glue: interface method sessionctx.Context.GetSessionVars
    fn get_session_vars_mut(&mut self) -> &mut SessionVars;
    /// `GetSessionVars().SQLKiller` (the session's shared part, N2 `SessionShared`).
    // glue: SessionVars.SQLKiller (N2 SessionShared)
    fn sql_killer(&self) -> Arc<SqlKiller>;
    /// `sessiontxn.GetTxnManager(sctx).GetStmtReadTS()`. Async: the provider may wait for
    /// the TSO its warm-up requested (`TxnContextProvider::get_stmt_read_ts` of D05.4 returns
    /// a future), which must not block the session's task.
    // glue: interface method sessiontxn.TxnManager.GetStmtReadTS (D05.3 / D05.4, VS1-14)
    fn get_stmt_read_ts(&mut self) -> BoxFuture<'_, Result<u64, Error>>;
    /// `sessiontxn.GetTxnManager(sctx).GetTxnInfoSchema()`.
    // glue: interface method sessiontxn.TxnManager.GetTxnInfoSchema (VS1-14)
    fn get_txn_info_schema(&mut self) -> Option<InfoSchemaRef>;
    /// `domain.GetDomain(sctx).InfoSchema()`: the newest InfoSchema, read by the metadata
    /// lock (`tryLockMDLAndUpdateSchemaIfNecessary`). None without a domain: a statement
    /// that takes the lock then stops at a boundary stub (D06.5, the session's domain).
    // glue: domain.GetDomain(sctx).InfoSchema() (D06.5)
    fn get_domain_info_schema(&self) -> Option<InfoSchemaRef> {
        None
    }
    /// `domain.GetDomain(sctx).GetPlanReplayerHandle()`: the plan replayer handle, whose task
    /// collector loads the capture tasks from `mysql.plan_replayer_task`. None: the plan
    /// replayer handle is not ported (its task collector and dump worker), so no capture task
    /// is ever loaded and statements on tidb-rs are not captured (docs/compatibility.md).
    // glue: stub of domain.Domain.GetPlanReplayerHandle, blocked-by:X (the plan replayer handle is not ported: no capture task is loaded)
    fn get_plan_replayer_handle(&self) -> Option<Arc<dyn PlanReplayerHandle>> {
        None
    }
    /// `sessiontxn.GetTxnManager(sctx).GetTxnScope()`.
    // glue: interface method sessiontxn.TxnManager.GetTxnScope (VS1-14)
    fn get_txn_scope(&self) -> String;
    /// `sessiontxn.GetTxnManager(sctx).GetReadReplicaScope()`.
    // glue: interface method sessiontxn.TxnManager.GetReadReplicaScope (VS1-14)
    fn get_read_replica_scope(&self) -> String;
    /// `staleread.IsStmtStaleness(sctx)`.
    // glue: sessiontxn/staleread.IsStmtStaleness (D05.4)
    fn is_stmt_staleness(&self) -> bool;
    /// `sctx.Txn(false)`: the start ts of the transaction when it is valid, `None` otherwise.
    // glue: interface method sessionctx.Context.Txn (VS1-14)
    fn txn_start_ts(&mut self) -> Result<Option<u64>, Error>;
    /// `SetValue(sessionctx.QueryString, text)`: the current statement's text in the
    /// session values. The default (contexts that are not a session) keeps nothing.
    // glue: interface method sessionctx.Context.SetValue with the key sessionctx.QueryString
    fn set_query_string(&mut self, text: String) {
        let _ = text;
    }
    /// `processinfoSetter.SetProcessInfo` when the session is one (Go type-asserts it).
    // glue: interface method processinfoSetter.SetProcessInfo (D05.3)
    fn set_process_info(&mut self, sql: &str, t: SystemTime, command: u8, max_execution_time: u64);
    /// `sessiontxn.AdviseOptimizeWithPlanAndThenWarmUp(sctx, plan)`: the MaxTS decision of an
    /// autocommit point get (`OptimisticTxnContextProvider.AdviseOptimizeWithPlan`) and the
    /// TSO prefetch.
    // glue: sessiontxn.AdviseOptimizeWithPlanAndThenWarmUp (D05.4, VS1-14)
    fn advise_optimize_with_plan_and_then_warm_up(&mut self, plan: &Plan) -> Result<(), Error>;
    /// `GetExprCtx()`: the session's expression context. An executor keeps it from the
    /// moment it is built (N2: executors do not hold the session).
    // glue: interface method sessionctx.Context.GetExprCtx (VS1-14)
    fn get_expr_ctx(&self) -> ExprContextRef;
    /// `GetDistSQLCtx()`: the session's `DistSQLContext` for coprocessor reads.
    // glue: interface method sessionctx.Context.GetDistSQLCtx (D05.3 / D07.3)
    fn get_dist_sql_ctx(&self) -> Arc<DistSQLContext>;
    /// `GetBuildPBCtx()`: the session's `BuildPBContext` for `PhysicalPlan.ToPB`.
    // glue: interface method sessionctx.Context.GetBuildPBCtx (D05.3 / D10.9)
    fn get_build_pb_ctx(&self) -> Result<Arc<BuildPBContext>, Error>;
    /// `GetRangerCtx()`: the session's `RangerContext` for range building.
    // glue: interface method planctx.PlanContext.GetRangerCtx (D05.3 / D12.1)
    fn get_ranger_ctx(&self) -> Arc<RangerContext>;
    /// `GetInfoSchema()`: the active info schema of the statement (transaction's if present,
    /// otherwise domain's).
    // glue: interface method planctx.PlanContext.GetInfoSchema (D05.3 / D12.1)
    fn get_info_schema(&self) -> Option<InfoSchemaRef> {
        self.get_domain_info_schema()
    }
    /// `GetPlanCtx()`: what the session hands the planner for a statement (its expression
    /// context, store client, the transaction's dirty-table check), from which
    /// `planner.Optimize` and PREPARE build the same planning context.
    // glue: interface method sessionctx.Context.GetPlanCtx (the session's parts of the planning context, N2)
    fn get_plan_ctx(&self) -> tidb_planner::planctx::SessionPlanCtx;
    /// `GetPlanCtx().SetReadonlyUserVarMap(m)`: the read-only user variables the
    /// preprocessor found, read by the expression context's `IsReadonlyUserVar`. The default
    /// is `EmptyPlanContextExtended`'s (nothing kept), for contexts that are not a session.
    // glue: interface method planctx.PlanContext.SetReadonlyUserVarMap
    fn set_readonly_user_var_map(&mut self, readonly_user_vars: std::collections::HashSet<String>) {
        tidb_sessionctx::planctx::EmptyPlanContextExtended
            .set_readonly_user_var_map(readonly_user_vars);
    }
    /// `GetPlanCtx().Reset()`: forgets the read-only user variables (`ResetContextOfStmt`).
    /// The default is `EmptyPlanContextExtended`'s.
    // glue: interface method planctx.PlanContext.Reset
    fn reset_plan_ctx(&mut self) {
        tidb_sessionctx::planctx::EmptyPlanContextExtended.reset();
    }
    /// `sessiontxn.GetTxnManager(sctx).GetSnapshotWithStmtReadTS()`. Async like
    /// [`SessionContext::get_stmt_read_ts`]: the provider may wait for the TSO.
    // glue: interface method sessiontxn.TxnManager.GetSnapshotWithStmtReadTS (D05.4, VS1-14)
    fn get_snapshot_with_stmt_read_ts(&mut self)
    -> BoxFuture<'_, Result<Arc<dyn Snapshot>, Error>>;
    /// The `ExecPool` the statement's executors start their tasks on (N3-A1 §3.1 items 3
    /// and 5, mapping rule M-GO): the pool the session's connection runs on, so that the
    /// executors' tasks get the time-sliced checkpoints, the stall detection and the CPU
    /// accounting of the pool's instrumentation. Go has no counterpart (its goroutines all
    /// run on the Go scheduler). `None` (the default, for contexts that are not a session and
    /// a session given no pool): the tasks go to the current tokio runtime, without that
    /// instrumentation.
    // glue: N3 ExecPool handle of the session (N3-A1 §3.1)
    fn exec_pool(&self) -> Option<tidb_runtime::pool::ExecPool> {
        None
    }
}

/// `resolve.ResultField`: one column of a result set, as the protocol layer describes it.
/// Go's `Column *model.ColumnInfo` and `Table *model.TableInfo` are reduced to the fields
/// `colNames2ResultFields` sets.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ResultField {
    /// `Column.Name`.
    pub column_name: CIStr,
    /// `Column.FieldType`.
    pub column_type: FieldType,
    pub column_as_name: CIStr,
    /// EmptyOrgName indicates whether this field has an empty org_name. A field has an empty
    /// org name, if it's an expression. It's not sure whether it's safe to use empty string
    /// in `.Column.Name`, so a new field is added to indicate whether it's empty.
    pub empty_org_name: bool,
    /// `Table.Name`.
    pub table_name: CIStr,
    pub table_as_name: CIStr,
    pub db_name: CIStr,
}

/// `plannercore.PlanCacheStmt` (D13.1): its `PointGet.Executor` holds the cached point get
/// executor as a `Box<Box<dyn Executor>>` (the planner, below this crate, keeps it as `Any`).
pub type PlanCacheStmt = tidb_planner::core::plan_cache_utils::PlanCacheStmt;

/// A prepared statement shared by the session's statement map and the statements that
/// execute it.
pub type PlanCacheStmtRef = tidb_planner::core::plan_cache_utils::PlanCacheStmtRef;

/// `stmt.PointGet.Executor = e`.
// glue: PointGetExecutorCache.Executor (an Any in the planner)
pub fn set_cached_point_get_executor(stmt: &mut PlanCacheStmt, e: Option<Box<dyn Executor>>) {
    stmt.point_get.executor = e.map(|e| Box::new(e) as Box<dyn std::any::Any + Send>);
}

/// `stmt.PointGet.Executor`, taken out of the statement.
// glue: PointGetExecutorCache.Executor (an Any in the planner)
pub fn take_cached_point_get_executor(stmt: &mut PlanCacheStmt) -> Option<Box<dyn Executor>> {
    stmt.point_get
        .executor
        .take()
        .and_then(|b| b.downcast::<Box<dyn Executor>>().ok())
        .map(|b| *b)
}

/// The `*PointGetExecutor` methods `ExecStmt.PointGet` calls on a cached executor (D12.1,
/// VS1-12): Go type-asserts `exec.Executor` to `*PointGetExecutor`; here the point get
/// executor returns itself from [`Executor::as_point_get`].
pub trait PointGetExecutor {
    /// `PointGetExecutor.Init(p)`: re-initialises a cached executor for a new execution.
    /// `sctx` is Go's `e.Ctx()`: the executor does not hold the session (N2).
    // glue: Go method PointGetExecutor.Init (D12.1)
    fn init(&mut self, sctx: &mut dyn SessionContext, plan: &Plan) -> Result<(), Error>;
    /// `partitionDefIdx != nil`.
    // glue: Go field PointGetExecutor.partitionDefIdx (D12.1)
    fn is_partitioned(&self) -> bool;
    /// Drops what the executor holds of the statement that just finished (its statement
    /// context, the base executor's runtime stats, the index usage reporter; the snapshot's
    /// runtime stats stay, as in Go) before the executor is kept in the
    /// prepared statement. Go's executor reaches the statement context through the session
    /// and `Init` replaces the rest; here the executor holds the statement context itself
    /// (`Arc`, N3), which the session writes after `Finish` (`FinishExecuteStmt`) and must
    /// then own alone. `Init` builds all of it again.
    // glue: N3 (the statement context an executor holds)
    fn release_stmt_state(&mut self);
}

/// client-go `txnsnapshot.SnapshotRuntimeStats`: the RPC counts, backoffs and TiKV time
/// details of a snapshot's reads. client-rust collects none (D07.1 spec, deviation 2), so the
/// stand-in is always empty: Go's methods on an empty value.
#[derive(Clone, Debug, Default)]
pub struct SnapshotRuntimeStats {}

impl SnapshotRuntimeStats {
    /// `String()` of empty stats.
    // glue: client-go SnapshotRuntimeStats.String
    pub fn string(&self) -> String {
        String::new()
    }
    /// `Merge`: nothing to merge.
    // glue: client-go SnapshotRuntimeStats.Merge
    pub fn merge(&mut self, other: &SnapshotRuntimeStats) {
        let _ = other;
    }
    /// `GetCmdRPCCount(tikvrpc.CmdGet)`: no RPC counted.
    // glue: client-go SnapshotRuntimeStats.GetCmdRPCCount
    pub fn get_cmd_rpc_count_get(&self) -> i64 {
        0
    }
}
