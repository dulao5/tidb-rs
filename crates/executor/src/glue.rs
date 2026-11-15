//! glue: what the executor framework (D12.0, VS1-13) needs from blocks above it or not ported
//! yet, as narrow interfaces and stand-in types (constitution §2.4).
//!
//! - [`SessionContext`]: the `sessionctx.Context` methods (and the `sessiontxn` transaction
//!   manager calls) that `ExecStmt`, `recordSet`, the compiler and the executor builder make.
//!   The session (D05.3, rank 15, above this crate) implements it in VS1-14, as it implements
//!   `tidb_sessiontxn::sctx::SessionCtx` (#219).
//! - [`ResultField`]: `resolve.ResultField` (`pkg/planner/core/resolve/result.go`, D09.0,
//!   VS1-10); moves to the planner when D09.0 lands.
//! - [`PlanCacheStmt`]: the part of `plannercore.PlanCacheStmt` (D13.1, VS1-15) that
//!   `ExecStmt.PointGet` reads, the cached point get executor.
//! - [`Plan`]: Go's `base.Plan`. The new stack only builds physical plans in VS1 (no
//!   `Execute`, `Set`, `Simple`, ... plans), so it is the physical plan.
//! - [`PointGetPlan`]: the fields of `plannercore.PointGetPlan` (D10.8, VS1-11) that the
//!   point get executor (D12.1, VS1-12) reads. VS1-11 replaces it with the planner's type
//!   (same field names); the executor builder downcasts the plan to it.
//! - [`encode_unique_index_key`] / [`encode_unique_index_values_for_key`]: the unique-index
//!   key encoding of `plannercore` (`util.go`, D09.0), boundary stubs.

use std::sync::Arc;
use std::time::SystemTime;

use futures::future::BoxFuture;
use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_expression::Schema;
use tidb_kv::key::Handle;
use tidb_kv::kv::Snapshot;
use tidb_memory::sqlkiller::SqlKiller;
use tidb_model::{ColumnInfo, IndexInfo, TableInfo};
use tidb_parser::model::CIStr;
use tidb_parser::types::FieldType;
use tidb_planner::physicalop::BasePhysicalPlan;
use tidb_planner::{PhysicalKind, PhysicalPlan, PhysicalPlanRef};
use tidb_sessionctx::exprctx::ExprContextRef;
use tidb_sessionctx::variable::session::SessionVars;
use tidb_types::datum::Datum;

use crate::internal::exec::executor::Executor;

/// `infoschema.InfoSchema` (the new stack only knows v2, VS1 plan §4.1).
pub type InfoSchemaRef = Arc<tidb_infoschema::InfoSchemaV2>;

/// Go's `base.Plan` (see the module comment).
pub type Plan = tidb_planner::PhysicalPlanRef;

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
    /// `sessiontxn.GetTxnManager(sctx).GetSnapshotWithStmtReadTS()`. Async like
    /// [`SessionContext::get_stmt_read_ts`]: the provider may wait for the TSO.
    // glue: interface method sessiontxn.TxnManager.GetSnapshotWithStmtReadTS (D05.4, VS1-14)
    fn get_snapshot_with_stmt_read_ts(&mut self)
    -> BoxFuture<'_, Result<Arc<dyn Snapshot>, Error>>;
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

/// `plannercore.PlanCacheStmt.PointGet`: the point get executor cached with a prepared
/// statement (`PointGet.Executor`).
#[derive(Default)]
pub struct PlanCacheStmt {
    /// `PointGet.Executor`.
    pub point_get_executor: Option<Box<dyn Executor>>,
}

/// A prepared statement's cache entry, shared by the session's statement map and the
/// statements that execute it.
pub type PlanCacheStmtRef = Arc<parking_lot::Mutex<PlanCacheStmt>>;

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
}

/// `plannercore.PointGetPlan` (`pkg/planner/core/point_get_plan.go`, D10.8, VS1-11): the
/// fields the point get executor reads, with Go's names. VS1-11 brings the real type (its
/// `Init`, `PrunePartitions`, `LoadTableStats`, ...) to the planner and removes this one.
///
/// Go's `PartitionIdx *int` is `Option<isize>`; `TblInfo` / `IndexInfo` / `Columns` share
/// the info schema's metadata (`plan-cache-clone:"shallow"`, N13).
#[derive(Clone, Default)]
pub struct PointGetPlan {
    pub base: BasePhysicalPlan,
    /// `ID()`.
    pub id: i32,
    /// `Schema()` (Go's `schema` field).
    pub schema: Schema,
    /// explicit partition selection
    pub partition_names: Vec<CIStr>,
    pub tbl_info: Arc<TableInfo>,
    pub index_info: Option<Arc<IndexInfo>>,
    pub partition_idx: Option<isize>,
    pub handle: Option<Handle>,
    pub index_values: Vec<Datum>,
    pub lock: bool,
    pub lock_wait_time: i64,
    pub columns: Vec<Arc<ColumnInfo>>,
}

impl PhysicalPlan for PointGetPlan {
    // glue: M-IFACE (the carrier's kind)
    fn kind(&self) -> PhysicalKind {
        PhysicalKind::PointGet
    }
    // glue: M-IFACE
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    // glue: M-IFACE
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    // glue: M-SELF
    fn as_dyn(&self) -> &dyn PhysicalPlan {
        self
    }
    // glue: M-SELF (Go PointGetPlan embeds baseimpl.Plan; the carrier keeps a base for the trait)
    fn base_physical(&self) -> &BasePhysicalPlan {
        &self.base
    }
    // glue: M-SELF
    fn base_physical_mut(&mut self) -> &mut BasePhysicalPlan {
        &mut self.base
    }
    // glue: N1 §3.5
    fn clone_node(&self) -> PhysicalPlanRef {
        Arc::new(self.clone())
    }
    /// `PointGetPlan.Schema()`: its own schema.
    // glue: accessor (Go PointGetPlan.Schema, D10.8)
    fn schema(&self) -> &Schema {
        &self.schema
    }
    // glue: accessor (Go PointGetPlan.SetSchema, D10.8)
    fn set_schema(&mut self, schema: Schema) {
        self.schema = schema;
    }
    // glue: accessor (Go baseimpl.Plan.ID)
    fn id(&self) -> i32 {
        self.id
    }
}

impl PointGetPlan {
    /// PrunePartitions will check which partition to use; returns true if no matching
    /// partition. A table without partitions needs no pruning (Go's first branch); the rest
    /// is VS1-11's (and partitioned tables are refused by the planner, VS1 plan §3.2).
    // glue: stub of PointGetPlan.PrunePartitions, blocked-by:D10.8 (partition pruning of a point get)
    pub fn prune_partitions(&self) -> Result<bool, Error> {
        if self.tbl_info.get_partition_info().is_none() {
            return Ok(false);
        }
        Err(blocked("D10.8", "partition pruning of a point get"))
    }

    /// `PointGetPlan.GetAvgRowSize()`: the row size estimate of the closest-adaptive replica
    /// read adjuster (cardinality, D10.6 / D10.8).
    // glue: stub of PointGetPlan.GetAvgRowSize, blocked-by:D10.8 (row size of a point get for tidb_replica_read=closest-adaptive)
    pub fn get_avg_row_size(&self) -> Result<f64, Error> {
        Err(blocked(
            "D10.8",
            "the row size of a point get (tidb_replica_read=closest-adaptive)",
        ))
    }
}

/// EncodeUniqueIndexKey encodes a unique index key (`pkg/planner/core/util.go`, D09.0).
/// Not in any VS1 work item's function list (VS1-12 spec §5): a unique-index point get stops
/// here.
// glue: stub of EncodeUniqueIndexKey, blocked-by:D09.0 (unique index key of a point get)
pub fn encode_unique_index_key(
    tbl_info: &TableInfo,
    idx_info: &IndexInfo,
    idx_vals: &[Datum],
    t_id: i64,
) -> Result<Vec<u8>, Error> {
    let _ = (tbl_info, idx_info, idx_vals, t_id);
    Err(blocked("D09.0", "the unique index key of a point get"))
}

/// EncodeUniqueIndexValuesForKey encodes unique index values for a key
/// (`pkg/planner/core/util.go`, D09.0; see [`encode_unique_index_key`]).
// glue: stub of EncodeUniqueIndexValuesForKey, blocked-by:D09.0 (clustered index handle of a point get)
pub fn encode_unique_index_values_for_key(
    tbl_info: &TableInfo,
    idx_info: &IndexInfo,
    idx_vals: &[Datum],
) -> Result<Vec<u8>, Error> {
    let _ = (tbl_info, idx_info, idx_vals);
    Err(blocked(
        "D09.0",
        "the clustered index handle of a point get on the primary key index",
    ))
}

/// `(&consistency.Reporter{...}).ReportLookupInconsistent(...)`
/// (`pkg/util/logutil/consistency`, D00.3): the error (and log) of an index entry whose
/// row is missing.
// glue: stub of Reporter.ReportLookupInconsistent, blocked-by:D00.3 (data inconsistency report of a point get)
pub fn report_lookup_inconsistent(tbl_info: &TableInfo, idx_info: &IndexInfo) -> Error {
    let _ = (tbl_info, idx_info);
    blocked(
        "D00.3",
        "the data inconsistency report of a unique index point get",
    )
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
