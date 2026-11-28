//! glue: what the session gives the blocks below it through their traits, and the stand-ins
//! for blocks above or beside it that are not ported yet (constitution §2.4).
//!
//! - [`StmtNode`] / [`PhysicalPlanInfo`]: the AST and the physical plan as sessiontxn sees
//!   them (`sctx::StmtInfo`, `sctx::PlanInfo`; D05.4 spec §2).
//! - [`UnwiredStore`]: `kv.Storage` / `oracle.Oracle` until the store driver's `Begin` and
//!   `GetOracle` are ported (D07.1); the PoC wiring uses it (spec §1.2).

use std::collections::HashMap;
use std::sync::Arc;

use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_kv::kv::{Client, KvFuture, Snapshot, Storage, Transaction, TxnOption};
use tidb_kv::version::Version;
use tidb_parser::ast;
use tidb_planner::{PhysicalKind, PhysicalPlan};
use tidb_sessionctx::variable::session::SessionVars;
use tidb_sessiontxn::sctx::{Oracle, PlanInfo, PlanKind, StmtInfo, TsFuture};

/// An AST statement as sessiontxn inspects it.
pub struct StmtNode<'a>(pub &'a ast::Stmt);

impl StmtInfo for StmtNode<'_> {
    /// `plannercore.IsReadOnly` (its `ast.IsReadOnly` is a boundary stub, `blocked-by:D02.2`,
    /// also for the statement of an `EXECUTE`); sessiontxn only
    /// asks it for the RC check-ts flag (`NeedSetRCCheckTSFlag`), where "not read-only"
    /// means "no RCCheckTS": the statement then reads at its own TSO, the safe answer. The
    /// RC provider is only used in pessimistic explicit or `autocommit=0` transactions,
    /// which the VS1 router keeps on the PoC.
    // glue: planner/core IsReadOnly (D09.x; blocked-by:D02.2 answers false)
    fn is_read_only(&self, vars: &SessionVars) -> bool {
        tidb_planner::optimize::is_read_only(self.0, vars).unwrap_or(false)
    }
}

/// A physical plan as sessiontxn inspects it (with its children, wrapped once).
pub struct PhysicalPlanInfo<'a> {
    plan: &'a dyn PhysicalPlan,
    children: Vec<PhysicalPlanInfo<'a>>,
}

impl<'a> PhysicalPlanInfo<'a> {
    /// Wraps `plan` and its subtree.
    // glue: constructor of the adapter
    pub fn new(plan: &'a dyn PhysicalPlan) -> PhysicalPlanInfo<'a> {
        PhysicalPlanInfo {
            plan,
            children: plan
                .children()
                .iter()
                .map(|c| PhysicalPlanInfo::new(c.as_dyn()))
                .collect(),
        }
    }
}

impl PlanInfo for PhysicalPlanInfo<'_> {
    /// The new stack's plans are physical; `PointGetPlan.Lock` comes with D10.8 (VS1-11),
    /// and `FOR UPDATE` never reaches the new stack (the router's AST pre-check), so it is
    /// false. `PhysicalLock` is not a VS1 operator.
    // glue: Go type switch on the plan (M-IFACE)
    fn plan_kind(&self) -> PlanKind<'_> {
        match self.plan.kind() {
            PhysicalKind::PointGet => PlanKind::PointGet { lock: false },
            _ => PlanKind::Physical {
                children: self.children.iter().map(|c| c as &dyn PlanInfo).collect(),
                is_lock: false,
            },
        }
    }
    /// Checks whether an autocommit single-point plan (`PointGet`, single-point `TableReader`,
    /// unique single-point `IndexReader`, or a `Projection` over one) may read at `MaxTS`
    /// (`common_plans.go:IsPointGetWithPKOrUniqueKeyByAutoCommit`).
    // glue: planner/core IsPointGetWithPKOrUniqueKeyByAutoCommit
    fn is_point_get_with_pk_or_unique_key_by_auto_commit(&self, vars: &SessionVars) -> bool {
        tidb_planner::core::common_plans::is_point_get_with_pk_or_unique_key_by_auto_commit(
            vars, self.plan,
        )
        .unwrap_or(false)
    }
}

/// The statement kind for logs and events (Go's statement label for the kinds the new stack
/// runs; `stmtctx.GetStmtLabel` is not ported, spec §6 item 3).
// glue: stand-in for stmtctx.GetStmtLabel (D12.0)
pub fn stmt_kind(node: &ast::Stmt) -> &'static str {
    match node {
        ast::Stmt::Select(_) => "Select",
        ast::Stmt::SetOpr(_) => "SetOpr",
        ast::Stmt::Begin(_) => "Begin",
        ast::Stmt::Commit(_) => "Commit",
        ast::Stmt::Rollback(_) => "Rollback",
        ast::Stmt::Use(_) => "Use",
        ast::Stmt::Set(_) => "Set",
        ast::Stmt::Show(_) => "Show",
        ast::Stmt::Execute(_) => "Execute",
        ast::Stmt::Unported(_) => "other",
    }
}

/// `stmtctx.GetStmtLabel(ctx, node)`: `"Select"` for a SELECT (what the new stack runs).
// glue: stub, blocked-by:D12.0 (stmtctx.GetStmtLabel, inherited from D00.1)
pub fn get_stmt_label(node: &ast::Stmt) -> Result<String, Error> {
    match node {
        ast::Stmt::Select(_) => Ok("Select".to_owned()),
        _ => Err(blocked("D12.0", "stmtctx.GetStmtLabel")),
    }
}

/// The store driver's PD oracle (`tikvStore.GetOracle()`) as the oracle sessiontxn calls.
// glue: oracle.Oracle of the store driver (the trait is sessiontxn's, the oracle the store's)
pub struct StoreOracle(pub tidb_store::driver::tikv::PdOracle);

impl Oracle for StoreOracle {
    // glue: dispatch (client-go pdOracle.GetTimestampAsync)
    fn get_timestamp_async(&self, txn_scope: &str) -> TsFuture {
        let oracle = self.0.clone();
        let scope = txn_scope.to_owned();
        Box::pin(async move {
            oracle
                .get_timestamp_async(&scope)
                .await
                .map_err(tidb_store::driver::error::to_tidb_err)
        })
    }
    // glue: dispatch (client-go pdOracle.GetLowResolutionTimestampAsync)
    fn get_low_resolution_timestamp_async(&self, txn_scope: &str) -> TsFuture {
        let oracle = self.0.clone();
        let scope = txn_scope.to_owned();
        Box::pin(async move {
            oracle
                .get_low_resolution_timestamp(&scope)
                .await
                .map_err(tidb_store::driver::error::to_tidb_err)
        })
    }
}

/// The error of the unwired store.
// glue: the error of the stand-in
fn unwired() -> Error {
    blocked(
        "D07.1",
        "the store driver's kv.Storage and oracle in the new session",
    )
}

/// `kv.Storage` and `oracle.Oracle` before the store driver's `Begin` / `GetOracle` exist.
#[derive(Default)]
pub struct UnwiredStore;

impl Oracle for UnwiredStore {
    // glue: stub, blocked-by:D07.1 (the store driver's oracle: Oracle.GetTimestampAsync)
    fn get_timestamp_async(&self, _txn_scope: &str) -> TsFuture {
        Box::pin(async { Err(unwired()) })
    }
    // glue: stub, blocked-by:D07.1 (the store driver's oracle: Oracle.GetLowResolutionTimestampAsync)
    fn get_low_resolution_timestamp_async(&self, _txn_scope: &str) -> TsFuture {
        Box::pin(async { Err(unwired()) })
    }
}

impl Storage for UnwiredStore {
    // glue: stub, blocked-by:D07.1 (the store driver's Begin: Storage.Begin)
    fn begin<'a>(&'a self, _opts: &'a [TxnOption]) -> KvFuture<'a, Box<dyn Transaction>> {
        Box::pin(async { Err(unwired()) })
    }
    // glue: stub, blocked-by:D07.1 (the store driver's snapshot: Storage.GetSnapshot)
    fn get_snapshot(&self, _ver: Version) -> Arc<dyn Snapshot> {
        Arc::new(UnwiredSnapshot)
    }
    // glue: stub, blocked-by:D07.1 (the store driver's client: Storage.GetClient)
    fn get_client(&self) -> Arc<dyn Client> {
        panic!("{}", unwired())
    }
    // glue: stand-in (nothing to close)
    fn close(&self) -> Result<(), Error> {
        Ok(())
    }
    // glue: stand-in
    fn uuid(&self) -> String {
        "unwired".to_owned()
    }
    // glue: stub, blocked-by:D07.1 (the store driver's oracle: Storage.CurrentVersion)
    fn current_version<'a>(&'a self, _txn_scope: &'a str) -> KvFuture<'a, Version> {
        Box::pin(async { Err(unwired()) })
    }
    // glue: stand-in
    fn name(&self) -> String {
        "unwired".to_owned()
    }
    // glue: stand-in
    fn describe(&self) -> String {
        "the store of the new session before D07.1".to_owned()
    }
    // glue: stand-in
    fn get_min_safe_ts(&self, _txn_scope: &str) -> u64 {
        0
    }
}

/// A snapshot of the unwired store: every read fails.
struct UnwiredSnapshot;

impl tidb_kv::kv::Getter for UnwiredSnapshot {
    // glue: stub, blocked-by:D07.1 (the store driver's snapshot: Snapshot.Get)
    fn get<'a>(
        &'a self,
        _: &'a tidb_kv::Context,
        _: &'a tidb_kv::key::Key,
        _: &'a [tidb_kv::kv::GetOption],
    ) -> KvFuture<'a, tidb_kv::kv::ValueEntry> {
        Box::pin(async { Err(unwired()) })
    }
}

impl tidb_kv::kv::Retriever for UnwiredSnapshot {
    // glue: stub, blocked-by:D07.1 (the store driver's snapshot: Snapshot.Iter)
    fn iter<'a: 'f, 'f>(
        &'a self,
        _: &'f tidb_kv::key::Key,
        _: &'f tidb_kv::key::Key,
    ) -> KvFuture<'f, Box<dyn tidb_kv::kv::Iterator + 'a>> {
        Box::pin(async { Err(unwired()) })
    }
    // glue: stub, blocked-by:D07.1 (the store driver's snapshot: Snapshot.IterReverse)
    fn iter_reverse<'a: 'f, 'f>(
        &'a self,
        _: &'f tidb_kv::key::Key,
        _: &'f tidb_kv::key::Key,
    ) -> KvFuture<'f, Box<dyn tidb_kv::kv::Iterator + 'a>> {
        Box::pin(async { Err(unwired()) })
    }
}

impl tidb_kv::kv::BatchGetter for UnwiredSnapshot {
    // glue: stub, blocked-by:D07.1 (the store driver's snapshot: Snapshot.BatchGet)
    fn batch_get<'a>(
        &'a self,
        _: &'a tidb_kv::Context,
        _: &'a [tidb_kv::key::Key],
        _: &'a [tidb_kv::kv::BatchGetOption],
    ) -> KvFuture<'a, HashMap<Vec<u8>, tidb_kv::kv::ValueEntry>> {
        Box::pin(async { Err(unwired()) })
    }
}

impl Snapshot for UnwiredSnapshot {
    // glue: stand-in (options of a snapshot that never reads)
    fn set_option(&self, _opt: isize, _val: tidb_kv::kv::OptionValue) {}
}
