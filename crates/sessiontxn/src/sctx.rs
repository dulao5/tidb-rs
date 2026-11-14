//! glue: the parts of the session, the store and the planner that `pkg/sessiontxn` calls.
//!
//! Go's sessiontxn holds a `sessionctx.Context` (the session, D05.3, crate `tidb-session`,
//! rank 15), calls `kv.Storage.GetOracle()` (the store driver, D07.1) and inspects planner
//! values (`planner/core`, D10.x, rank 13). All of them are above this crate (rank 11,
//! N0), so the package's calls are traits here that those crates implement (D05.4 spec
//! §2). Each method is one Go call; none adds behaviour.
//!
//! The session is passed to every provider method instead of being stored in the provider
//! (N2: the session owns its transaction manager, which owns the provider).

use std::sync::Arc;

use futures::future::BoxFuture;
use tidb_base::Error;
use tidb_infoschema::InfoSchemaV2;
use tidb_kv::kv::{OptionValue, Snapshot, Storage};
use tidb_sessionctx::variable::session::SessionVars;
use tidb_sessionctx::variable::session_state::KVVars;

/// The information schema of a transaction (Go `infoschema.InfoSchema`). The interface
/// comes with D06.3 part 2 (T9); until then the v2 InfoSchema itself.
pub type InfoSchemaRef = Arc<InfoSchemaV2>;

/// A timestamp future (Go `oracle.Future`): waited once.
pub type TsFuture = BoxFuture<'static, Result<u64, Error>>;

/// The future a transaction is prepared with (`LazyTxn.PrepareTSFuture`): Go passes an
/// `oracle.Future`; the two kinds sessiontxn passes.
pub enum TxnTsFuture {
    /// `sessiontxn.ConstantFuture` (`tidb_snapshot`, the point-get MaxTS).
    Constant(u64),
    /// A TSO request (`newOracleFuture`).
    Oracle(TsFuture),
}

/// `oracle.Oracle`: the TSO client of the store (client-go; client-rust, ADR 0008).
pub trait Oracle: Send + Sync {
    /// `GetTimestampAsync(ctx, &oracle.Option{TxnScope: scope})`.
    // glue: interface method oracle.Oracle.GetTimestampAsync
    fn get_timestamp_async(&self, txn_scope: &str) -> TsFuture;
    /// `GetLowResolutionTimestampAsync(ctx, &oracle.Option{TxnScope: scope})`.
    // glue: interface method oracle.Oracle.GetLowResolutionTimestampAsync
    fn get_low_resolution_timestamp_async(&self, txn_scope: &str) -> TsFuture;
}

/// The session's transaction (`*session.LazyTxn` as `kv.Transaction`): the methods
/// sessiontxn calls.
pub trait LazyTxn: Send {
    // glue: interface method kv.Transaction.Valid (LazyTxn)
    fn valid(&self) -> bool;
    // glue: interface method kv.Transaction.StartTS (LazyTxn)
    fn start_ts(&self) -> u64;
    // glue: interface method kv.Transaction.GetSnapshot (LazyTxn)
    fn get_snapshot(&self) -> Arc<dyn Snapshot>;
    // glue: interface method kv.Transaction.SetOption (LazyTxn)
    fn set_option(&mut self, opt: isize, val: OptionValue);
    // glue: interface method kv.Transaction.SetVars (LazyTxn)
    fn set_vars(&mut self, vars: &KVVars);
    // glue: interface method kv.Transaction.IsPipelined (LazyTxn)
    fn is_pipelined(&self) -> bool;
}

/// `sessionctx.Context`: the methods sessiontxn calls (implemented by the session, D05.3).
pub trait SessionCtx: Send {
    // glue: interface method sessionctx.Context.GetSessionVars
    fn get_session_vars(&self) -> &SessionVars;
    // glue: interface method sessionctx.Context.GetSessionVars (written through the pointer)
    fn get_session_vars_mut(&mut self) -> &mut SessionVars;
    /// `Txn(false)`: the transaction, valid or not (never activates it).
    // glue: interface method sessionctx.Context.Txn(false)
    fn txn(&mut self) -> &mut dyn LazyTxn;
    /// `GetPreparedTxnFuture() != nil`.
    // glue: interface method sessionctx.Context.GetPreparedTxnFuture
    fn has_prepared_txn_future(&self) -> bool;
    /// `GetPreparedTxnFuture().Wait(ctx, sctx)`: activates the transaction with the
    /// prepared future; afterwards [`SessionCtx::txn`] is valid.
    // glue: interface method sessionctx.TxnFuture.Wait
    fn wait_prepared_txn_future(&mut self) -> BoxFuture<'_, Result<(), Error>>;
    /// `PrepareTSFuture(ctx, future, scope)`.
    // glue: interface method sessionctx.Context.PrepareTSFuture
    fn prepare_ts_future(&mut self, future: TxnTsFuture, scope: &str) -> Result<(), Error>;
    /// `CommitTxn(ctx)`.
    // glue: interface method sessionctx.Context.CommitTxn
    fn commit_txn(&mut self) -> BoxFuture<'_, Result<(), Error>>;
    /// `GetDomainInfoSchema()`.
    // glue: interface method sessionctx.Context.GetDomainInfoSchema
    fn get_domain_info_schema(&self) -> InfoSchemaRef;
    /// `GetStore()`.
    // glue: interface method sessionctx.Context.GetStore
    fn get_store(&self) -> Arc<dyn Storage>;
    /// `GetStore().GetOracle()`.
    // glue: interface method kv.Storage.GetOracle (D07.1)
    fn get_oracle(&self) -> Arc<dyn Oracle>;
}

/// A statement node as sessiontxn inspects it (Go `ast.StmtNode`, passed to the planner).
pub trait StmtInfo {
    /// `plannercore.IsReadOnly(node, vars)`.
    // glue: planner/core IsReadOnly (D09.x)
    fn is_read_only(&self, vars: &SessionVars) -> bool;
}

/// The planner values sessiontxn's type switches look at (M-IFACE: `kind()` enum match).
pub enum PlanKind<'a> {
    /// `*plannercore.Execute`, with its `Plan`.
    Execute(&'a dyn PlanInfo),
    /// `*plannercore.PointGetPlan`, with `Lock`.
    PointGet { lock: bool },
    /// `*plannercore.BatchPointGetPlan`, with `Lock`.
    BatchPointGet { lock: bool },
    /// Another `base.PhysicalPlan`: its children and whether it is a `*PhysicalLock`.
    Physical {
        children: Vec<&'a dyn PlanInfo>,
        is_lock: bool,
    },
    /// `*plannercore.Update`, with its `SelectPlan`.
    Update(&'a dyn PlanInfo),
    /// `*plannercore.Delete`, with its `SelectPlan`.
    Delete(&'a dyn PlanInfo),
    /// `*plannercore.Insert`: whether `SelectPlan` is set, `len(OnDuplicate) > 0`, `IsReplace`.
    Insert {
        has_select_plan: bool,
        has_on_duplicate: bool,
        is_replace: bool,
    },
    /// Any other `base.Plan`.
    Other,
}

/// A planner value (`base.Plan`) as sessiontxn inspects it.
pub trait PlanInfo {
    // glue: Go type switch on the plan (M-IFACE)
    fn plan_kind(&self) -> PlanKind<'_>;
    /// `plannercore.IsPointGetWithPKOrUniqueKeyByAutoCommit(vars, plan)` (D10.1).
    // glue: planner/core IsPointGetWithPKOrUniqueKeyByAutoCommit (D10.1)
    fn is_point_get_with_pk_or_unique_key_by_auto_commit(&self, vars: &SessionVars) -> bool;
}

/// Where a statement's read timestamp comes from (N11 §4.4, the tidb-rs carrier of the
/// providers' choice; the schema lease check at admission, N11 §4.2 / ADR 0005, only
/// accepts a fresh `Tso` of the statement, never the transaction's start ts).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadTsSource {
    /// A new TSO (a future: issued before planning, waited after).
    Tso,
    /// `tidb_low_resolution_tso`.
    LowResolution,
    /// The autocommit point get (`IsPointGetWithPKOrUniqueKeyByAutoCommit`).
    MaxTs,
    /// RR / optimistic explicit transactions: the transaction's start ts.
    TxnStart(u64),
    /// AS OF / `tx_read_ts` / `tidb_read_staleness` / external ts.
    Stale(u64),
    /// `tidb_snapshot`.
    Snapshot(u64),
}
