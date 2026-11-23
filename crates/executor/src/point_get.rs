//! go-file: pkg/executor/point_get.go
//!
//! The point get executor (block D12.1, VS1-12): reads one row by its handle (or by a unique
//! index key, then the handle) from the statement's snapshot and decodes it into the chunk.
//!
//! Carriers:
//! - **The session (N2).** Go's executor keeps `e.Ctx()` and reads the session in `Next`.
//!   Here what `Next` reads from the session is taken in `Init` (built and re-initialised by
//!   a session that is passed in): the expression context, the time zone, the
//!   pessimistic-read-consistency flag, `tidb_enable_point_get_cache`. The statement context
//!   is the executor's `ExecCtx`.
//! - **The plan** is the planner's [`PointGetPlan`] (D10.8, VS1-11).
//! - **No locking** (VS1 plan §3: the point get without locks). `SELECT ... FOR UPDATE`
//!   (`p.Lock`) and the for-update snapshot are the write path (block X): the builder stops
//!   with `blocked-by:X` before the executor runs. The lock helpers are ported for their
//!   non-locking paths; their locking branches are the same stub.
//! - **The transaction's mem buffer** (`e.txn`): a tidb-rs transaction never writes (the
//!   node is read-only, N11), so Go's `txn.Valid() && !txn.IsReadOnly()` is always false and
//!   reads go to the snapshot.
//! - The FieldTypes of the decoder are the schema columns' (`Arc`, N13): taken once per
//!   `Init`, borrowed by every row decode.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_base::gostd::time::Location;
use tidb_base::mysql::{self, SQLMode};
use tidb_chunk::chunk::{Chunk, new_chunk_with_capacity};
use tidb_codec::codec::{Decoder, new_decoder};
use tidb_execdetails::execdetails::{RuntimeStats, TpRuntimeStatsWithSnapshot};
use tidb_expression::{Column, Schema};
use tidb_infoschema::table::column::{fill_virtual_column_value, get_col_origin_default_value};
use tidb_infoschema::tables::{
    find_primary_index, primary_prefix_column_ids, try_get_common_pk_column_ids,
};
use tidb_kv::context::Context;
use tidb_kv::error::ErrNotExist;
use tidb_kv::key::{Handle, Key, new_common_handle};
use tidb_kv::kv::{self as kvi, Snapshot};
use tidb_kv::option as kvopt;
use tidb_kv::txn_scope_var::GlobalTxnScope;
use tidb_model::{
    ColumnInfo, ExtraHandleID, ExtraRowChecksumID, IndexInfo, PartitionInfo,
    TableCacheStatusEnable, TableInfo, TempTableNone,
};
use tidb_parser::model::{CIStr, TableLockRead, TableLockReadOnly};
use tidb_parser::types::FieldType;
use tidb_sessionctx::exprctx::{BuildContext, ExprContextRef};
use tidb_tablecodec::rowcodec::common::{ColData, RowData, is_new_format};
use tidb_tablecodec::rowcodec::decoder::ChunkDecoder;
use tidb_tablecodec::tablecodec;
use tidb_types::datum::Datum;
use tidb_types::etc::need_restored_data;

use crate::builder::{
    ExecutorBuilder, build_index_usage_reporter, is_common_handle_read, new_replica_read_adjuster,
    new_row_decoder,
};
use crate::consistency::Reporter;
use crate::exec::ExecCtx;
use crate::glue::{self, Plan, SessionContext, SnapshotRuntimeStats};
use crate::internal::exec::executor::{BaseExecutor, BaseExecutorV2, Executor, new_base_executor};
use crate::internal::exec::indexusage::IndexUsageReporter;
use crate::table_reader::build_virtual_column_index;
use tidb_planner::PhysicalPlan;
use tidb_planner::core::point_get_plan::PointGetPlan;
use tidb_planner::core::util::{encode_unique_index_key, encode_unique_index_values_for_key};

impl ExecutorBuilder {
    /// The point get executor of `p` (`None` with `b.err` set on failure, as Go).
    ///
    /// Differences from Go (all because of the carriers above):
    /// - `p.Lock` (`SELECT ... FOR UPDATE`) is `blocked-by:X` (no locking point get);
    /// - a `TableDual` for a pruned-away partition comes with partitioned tables (pruning
    ///   by value is D06.4 part 2's); the plan of a partitioned table is pruned on a copy;
    /// - `StmtCtx.IndexNames` is written before the executor shares the statement context.
    // go: executorBuilder.buildPointGet L2 (no locking: p.Lock is blocked-by:X; IndexNames written before the executor shares the statement context)
    pub async fn build_point_get(
        &mut self,
        sctx: &mut dyn SessionContext,
        p: &PointGetPlan,
    ) -> Option<Box<dyn Executor>> {
        if let Err(err) = self.valid_can_read_temporary_or_cache_table(sctx, &p.tbl_info) {
            self.err = Some(err);
            return None;
        }

        // Go prunes on the plan itself (`PrunePartitions` sets `PartitionIdx` on the cached
        // plan); the plan is shared here (N1 §3.4), so a partitioned table's plan is pruned on
        // a copy the executor reads. A table without partitions needs no pruning.
        let pruned;
        let p = if p.tbl_info.get_partition_info().is_none() {
            p
        } else {
            let mut q = p.clone();
            let use_cache = sctx
                .get_session_vars()
                .stmt_ctx()
                .plan_cache_tracker
                .use_cache();
            match q.prune_partitions(use_cache) {
                Ok(true) => {
                    // TableDualExec with no rows (D12.2, VS1-35).
                    self.err = Some(blocked("D12.2", "the table dual executor (VS1-35)"));
                    return None;
                }
                Ok(false) => {}
                Err(err) => {
                    self.err = Some(err);
                    return None;
                }
            }
            pruned = q;
            &pruned
        };

        if p.lock && p.tbl_info.temp_table_type == TempTableNone {
            // `inSelectLockStmt`, then the pessimistic lock of the read key (Init sets e.lock
            // from p.Lock for a non-temporary table): the write path.
            self.err = Some(blocked("X", "SELECT ... FOR UPDATE on a point get"));
            return None;
        }

        sctx.get_session_vars()
            .stmt_ctx()
            .is_tikv
            .store(true, std::sync::atomic::Ordering::SeqCst);

        if let Some(idx) = &p.index_info {
            // Written here, before the executor below shares the statement context (Go
            // appends after building it; nothing reads IndexNames in between).
            let name = format!("{}:{}", p.tbl_info.name.o, idx.name.o);
            sctx.get_session_vars_mut()
                .stmt_ctx_mut()
                .index_names
                .push(name);
        }

        let index_usage_reporter = self.build_index_usage_reporter(sctx, p);
        let mut e = PointGetExecutor::new(sctx, p, index_usage_reporter);
        e.txn_scope = self.txn_scope.clone();
        e.read_replica_scope = self.read_replica_scope.clone();
        e.is_staleness = self.is_staleness;
        e.partition_names = p.partition_names.clone();

        e.base.base.alloc.set_init_cap(1);
        e.base.base.alloc.set_max_chunk_size(1);
        e.init(sctx, p);

        let snapshot = match self.get_snapshot(sctx).await {
            Ok(s) => s,
            Err(err) => {
                self.err = Some(err);
                return None;
            }
        };
        if sctx.get_session_vars().is_replica_read_closest_adaptive() {
            let avg_row_size = match p.get_avg_row_size() {
                Ok(s) => s,
                Err(err) => {
                    self.err = Some(err);
                    return None;
                }
            };
            snapshot.set_option(
                kvopt::ReplicaReadAdjuster,
                Box::new(new_replica_read_adjuster(sctx, avg_row_size)),
            );
        }
        if e.runtime_stats().is_some() {
            // `SnapshotRuntimeStats` (client-go): client-rust collects none, the stand-in
            // stays empty (glue::SnapshotRuntimeStats).
            e.stats = Some(RuntimeStatsWithSnapshot {
                snapshot_runtime_stats: Some(SnapshotRuntimeStats::default()),
            });
            snapshot.set_option(kvopt::CollectRuntimeStats, Box::new(Some(())));
        }
        e.snapshot = Some(snapshot);

        // The "assertPointReplicaOption" failpoint is not ported.

        let snapshot_ts = match self.get_snapshot_ts(sctx).await {
            Ok(ts) => ts,
            Err(err) => {
                self.err = Some(err);
                return None;
            }
        };
        if p.tbl_info.table_cache_status_type == TableCacheStatusEnable {
            // The cache table snapshot (`cacheTableSnapshot`): D06.4 part 2.
            self.get_cache_table(&p.tbl_info, snapshot_ts)?;
        }

        if e.lock {
            self.has_lock = true;
        }

        Some(Box::new(e))
    }
}

/// What `Next` reads from the session (Go's `e.Ctx()`), taken by `Init` (N2).
#[derive(Clone)]
struct SessionState {
    /// `sctx.GetExprCtx()`.
    expr_ctx: ExprContextRef,
    /// `sctx.GetSessionVars().Location()`.
    location: Arc<Location>,
    /// `sctx.GetSessionVars().IsPessimisticReadConsistency()`.
    pessimistic_read_consistency: bool,
    /// `sctx.GetSessionVars().EnablePointGetCache`.
    enable_point_get_cache: bool,
    /// `sctx.GetSessionVars().SQLMode` and `.ConnectionID`: `table.CastValue` of the unique
    /// index values (`EncodeUniqueIndexValuesForKey`).
    sql_mode: SQLMode,
    connection_id: u64,
    /// `sctx.GetSessionVars().EnableRedactLog` (the inconsistency report).
    enable_redact_log: String,
}

impl SessionState {
    // glue: N2 (the session state an executor keeps)
    fn of(sctx: &mut dyn SessionContext) -> SessionState {
        let expr_ctx = sctx.get_expr_ctx();
        let vars = sctx.get_session_vars_mut();
        SessionState {
            expr_ctx,
            location: vars.location(),
            pessimistic_read_consistency: vars.is_pessimistic_read_consistency(),
            enable_point_get_cache: vars.enable_point_get_cache,
            sql_mode: SQLMode(vars.sql_mode),
            connection_id: vars.connection_id,
            enable_redact_log: vars.enable_redact_log.clone(),
        }
    }
}

/// PointGetExecutor executes point select query.
pub struct PointGetExecutor {
    base: BaseExecutor,
    index_usage_reporter: Option<IndexUsageReporter>,

    tbl_info: Arc<TableInfo>,
    handle: Option<Handle>,
    idx_info: Option<Arc<IndexInfo>>,
    partition_def_idx: Option<isize>,
    partition_names: Vec<CIStr>,
    /// Go's nil key is `None`.
    idx_key: Option<Key>,
    handle_val: Vec<u8>,
    idx_vals: Vec<Datum>,
    txn_scope: String,
    read_replica_scope: String,
    is_staleness: bool,
    snapshot: Option<Arc<dyn Snapshot>>,
    done: bool,
    lock: bool,
    lock_wait_time: i64,
    row_decoder: Option<ChunkDecoder>,

    columns: Vec<Arc<ColumnInfo>>,
    /// virtualColumnIndex records all the indices of virtual columns and sort them in
    /// definition to make sure we can compute the virtual column in right order.
    virtual_column_index: Vec<usize>,

    /// virtualColumnRetFieldTypes records the RetFieldTypes of virtual columns (the schema
    /// columns' `RetType`, shared).
    virtual_column_ret_field_types: Vec<Arc<FieldType>>,

    stats: Option<RuntimeStatsWithSnapshot>,

    sess: SessionState,
}

/// GetPhysID returns the physical id used, either the table's id or a partition's ID
// go: GetPhysID L1
pub fn get_phys_id(tbl_info: &TableInfo, idx: Option<isize>) -> i64 {
    if let Some(idx) = idx {
        if idx < 0 {
            // intest.Assert(false): a test-build assertion only.
            debug_assert!(idx >= 0, "GetPhysID: negative partition index");
        } else if let Some(pi) = tbl_info.get_partition_info() {
            return pi.definitions.as_deref().unwrap_or(&[])[idx as usize].id;
        }
    }
    tbl_info.id
}

// go: matchPartitionNames L1
fn match_partition_names(pid: i64, partition_names: &[CIStr], pi: &PartitionInfo) -> bool {
    if partition_names.is_empty() {
        return true;
    }
    let defs = pi.definitions.as_deref().unwrap_or(&[]);
    for def in defs {
        // TODO: create a map from id to partition definition index
        if def.id == pid {
            for name in partition_names {
                if def.name.l == name.l {
                    return true;
                }
            }
            // Only one partition can match pid
            return false;
        }
    }
    false
}

impl PointGetExecutor {
    /// The executor before `Init` (Go's struct literal in `buildPointGet`).
    // glue: Go struct literal of PointGetExecutor in buildPointGet
    fn new(
        sctx: &mut dyn SessionContext,
        p: &PointGetPlan,
        index_usage_reporter: Option<IndexUsageReporter>,
    ) -> PointGetExecutor {
        let killer = sctx.sql_killer();
        PointGetExecutor {
            base: new_base_executor(
                sctx.get_session_vars(),
                &killer,
                Some(p.schema.clone()),
                p.id() as isize,
                Vec::new(),
            ),
            index_usage_reporter,
            tbl_info: Arc::clone(&p.tbl_info),
            handle: None,
            idx_info: None,
            partition_def_idx: None,
            partition_names: Vec::new(),
            idx_key: None,
            handle_val: Vec::new(),
            idx_vals: Vec::new(),
            txn_scope: String::new(),
            read_replica_scope: String::new(),
            is_staleness: false,
            snapshot: None,
            done: false,
            lock: false,
            lock_wait_time: 0,
            row_decoder: None,
            columns: Vec::new(),
            virtual_column_index: Vec::new(),
            virtual_column_ret_field_types: Vec::new(),
            stats: None,
            sess: SessionState::of(sctx),
        }
    }

    /// Init set fields needed for PointGetExecutor reuse, this does NOT change baseExecutor
    /// field
    // go: PointGetExecutor.Init L1 [M-CTX, M-SHARE]
    pub fn init(&mut self, sctx: &mut dyn SessionContext, p: &PointGetPlan) {
        self.sess = SessionState::of(sctx);
        let decoder = new_row_decoder(
            &self.sess.expr_ctx,
            Arc::clone(&self.sess.location),
            &p.schema,
            &p.tbl_info,
        );
        self.tbl_info = Arc::clone(&p.tbl_info);
        self.handle = p.handle.clone();
        self.idx_info = p.index_info.clone();
        self.idx_vals = p.index_values.clone();
        self.done = false;
        if self.tbl_info.temp_table_type == TempTableNone {
            self.lock = p.lock;
            self.lock_wait_time = p.lock_wait_time;
        } else {
            // Temporary table should not do any lock operations
            self.lock = false;
            self.lock_wait_time = 0;
        }
        self.row_decoder = Some(decoder);
        self.partition_def_idx = p.partition_idx;
        self.columns = p.columns.clone();
        self.build_virtual_column_info();

        // It's necessary to at least reset the `runtimeStats` of the `BaseExecutor`.
        // As the `StmtCtx` may have changed, a new index usage reporter should also be created.
        let killer = sctx.sql_killer();
        self.base = new_base_executor(
            sctx.get_session_vars(),
            &killer,
            Some(p.schema.clone()),
            p.id() as isize,
            Vec::new(),
        );
        self.index_usage_reporter = build_index_usage_reporter(sctx, p);
    }

    /// buildVirtualColumnInfo saves virtual column indices and sort them in definition order
    // go: PointGetExecutor.buildVirtualColumnInfo L1 [M-SHARE]
    fn build_virtual_column_info(&mut self) {
        let schema = self.base.base.meta.schema();
        self.virtual_column_index = build_virtual_column_index(&schema, &self.columns);
        self.virtual_column_ret_field_types.clear();
        if !self.virtual_column_index.is_empty() {
            self.virtual_column_ret_field_types = self
                .virtual_column_index
                .iter()
                .map(|&idx| Arc::clone(&schema.columns[idx].ret_type))
                .collect();
        }
    }

    /// The snapshot of the statement (set by `buildPointGet`).
    // glue: accessor (Go reads e.snapshot)
    fn snapshot(&self) -> &Arc<dyn Snapshot> {
        self.snapshot
            .as_ref()
            .expect("a built point get executor has a snapshot")
    }

    // go: PointGetExecutor.getAndLock L1
    async fn get_and_lock(&mut self, key: &Key) -> Result<Option<Vec<u8>>, Error> {
        if self.sess.pessimistic_read_consistency {
            // Only Lock the existing keys in RC isolation.
            if self.lock {
                return self.lock_key_if_exists(Some(key));
            }
            return match self.get(Some(key)).await {
                Ok(v) => Ok(Some(v)),
                Err(err) => {
                    if !ErrNotExist.equal(&err) {
                        return Err(err);
                    }
                    Ok(None)
                }
            };
        }
        // Lock the key before get in RR isolation, then get will get the value from the cache.
        self.lock_key_if_needed(Some(key))?;
        match self.get(Some(key)).await {
            Ok(v) => Ok(Some(v)),
            Err(err) => {
                if !ErrNotExist.equal(&err) {
                    return Err(err);
                }
                Ok(None)
            }
        }
    }

    // go: PointGetExecutor.lockKeyIfNeeded L1
    fn lock_key_if_needed(&mut self, key: Option<&Key>) -> Result<(), Error> {
        self.lock_key_base(key, false)?;
        Ok(())
    }

    /// lockKeyIfExists locks the key if needed, but won't lock the key if it doesn't exis.
    /// Returns the value of the key if the key exist.
    // go: PointGetExecutor.lockKeyIfExists L1
    fn lock_key_if_exists(&mut self, key: Option<&Key>) -> Result<Option<Vec<u8>>, Error> {
        self.lock_key_base(key, true)
    }

    /// The pessimistic lock of the key when `e.lock` (`newLockCtx`, `doLockKeys`, the lock
    /// cache): the write path. The builder refuses `p.Lock`, so the branch is not reached.
    // go: PointGetExecutor.lockKeyBase L2 (the locking branch is blocked-by:X)
    fn lock_key_base(
        &mut self,
        key: Option<&Key>,
        lock_only_if_exists: bool,
    ) -> Result<Option<Vec<u8>>, Error> {
        let _ = lock_only_if_exists;
        if key.is_none_or(|k| k.0.is_empty()) {
            return Ok(None);
        }

        if self.lock {
            return Err(blocked("X", "the pessimistic lock of a point get"));
        }

        Ok(None)
    }

    /// get will first try to get from txn buffer, then check the pessimistic lock cache,
    /// then the store. Kv.ErrNotExist will be returned if key is not found
    ///
    /// `&mut self`: the future must be `Send`, and the executor is not `Sync`.
    // go: PointGetExecutor.get L2 (no transaction mem buffer: tidb-rs transactions never write; the read-locked table cache is blocked-by:X)
    async fn get(&mut self, key: Option<&Key>) -> Result<Vec<u8>, Error> {
        let Some(key) = key.filter(|k| !k.0.is_empty()) else {
            return Err(ErrNotExist.clone());
        };

        // `e.txn.Valid() && !e.txn.IsReadOnly()`: the transaction's mem buffer and the
        // pessimistic lock cache hold its writes and locks; a tidb-rs transaction has none.

        if let Some(lock) = &self.tbl_info.lock
            && (lock.tp == TableLockRead || lock.tp == TableLockReadOnly)
            && self.sess.enable_point_get_cache
        {
            // cacheDB.UnionGet: the store's memory cache of tables under LOCK TABLES READ.
            return Err(blocked(
                "X",
                "the point get cache of a table locked by LOCK TABLES",
            ));
        }
        // if not read lock or table was unlock then snapshot get
        kvi::get_value(&Context::background(), &**self.snapshot(), key).await
    }

    /// The transaction scope check of a local (non-global) transaction.
    // go: PointGetExecutor.verifyTxnScope L2 (a local txn scope needs distsql.VerifyTxnScope: blocked-by:D07.3)
    fn verify_txn_scope(&self) -> Result<(), Error> {
        if self.txn_scope.is_empty() || self.txn_scope == GlobalTxnScope {
            return Ok(());
        }
        // is.TableByID + distsql.VerifyTxnScope (placement rules of local transactions).
        Err(blocked("D07.3", "a point get in a local transaction scope"))
    }
}

#[async_trait]
impl Executor for PointGetExecutor {
    // glue: M-SELF
    fn base(&self) -> &BaseExecutorV2 {
        &self.base.base
    }
    // glue: M-SELF
    fn base_mut(&mut self) -> &mut BaseExecutorV2 {
        &mut self.base.base
    }

    /// Open implements the Executor interface.
    ///
    /// Go reads `e.Ctx().Txn(false)` for the mem buffer of `get` (none in tidb-rs, see the
    /// module comment) and sets the Top SQL options of the snapshot (`setOptionForTopSQL`,
    /// D12.2: the resource group tagger is Top SQL's, D14.2, not recorded; the snapshot
    /// ignores it).
    // go: PointGetExecutor.Open L2 (no transaction read: no mem buffer; no Top SQL options, D14.2)
    async fn open(&mut self, _ctx: &ExecCtx) -> Result<(), Error> {
        self.verify_txn_scope()?;
        Ok(())
    }

    /// Close implements the Executor interface.
    // go: PointGetExecutor.Close L2 (the registered stats are a copy: Go registers its pointer)
    async fn close(&mut self) -> Result<(), Error> {
        if let Some(stats) = &self.stats
            && let Some(sc) = self.base.ctx()
            && let Some(coll) = &sc.runtime_stats_coll
        {
            // deferred in Go (after the index usage report): the order is not observable.
            coll.register_stats(self.base.base.meta.id() as i64, Box::new(stats.clone()));
            // timeDetail := e.stats.SnapshotRuntimeStats.GetTimeDetail(): client-rust gives
            // none, so no TiKV CPU time is merged into SQLCPUUsages.
        }
        if self.runtime_stats().is_some()
            && let Some(snapshot) = &self.snapshot
        {
            snapshot.set_option(kvopt::CollectRuntimeStats, Box::new(None::<()>));
        }
        if let Some(reporter) = &self.index_usage_reporter {
            let table_id = self.tbl_info.id;
            let physical_table_id = get_phys_id(&self.tbl_info, self.partition_def_idx);
            let kv_req_total = self
                .stats
                .as_ref()
                .and_then(|s| s.snapshot_runtime_stats.as_ref())
                .map(|s| s.get_cmd_rpc_count_get())
                .unwrap_or(0);
            let rows = self.runtime_stats().map(|s| s.get_act_rows()).unwrap_or(0);
            if let Some(idx_info) = &self.idx_info {
                reporter.report_point_get_index_usage(
                    table_id,
                    physical_table_id,
                    idx_info.id,
                    kv_req_total,
                    rows,
                );
            } else {
                reporter.report_point_get_index_usage_for_handle(
                    &self.tbl_info,
                    physical_table_id,
                    kv_req_total,
                    rows,
                );
            }
        }
        self.done = false;
        Ok(())
    }

    /// Next implements the Executor interface.
    // go: PointGetExecutor.Next L2 (no locking; Go's ctx is a background KV context, M-GOCTX)
    async fn next(&mut self, ctx: &ExecCtx, req: &mut Chunk) -> Result<(), Error> {
        req.reset();
        if self.done {
            return Ok(());
        }
        self.done = true;

        let mut tbl_id = get_phys_id(&self.tbl_info, self.partition_def_idx);
        if self.lock {
            self.base.update_delta_for_table_id(tbl_id)?;
        }
        if let Some(idx_info) = self.idx_info.clone() {
            if is_common_handle_read(&self.tbl_info, &idx_info) {
                let handle_bytes = match encode_unique_index_values_for_key(
                    &ctx.stmt,
                    self.sess.sql_mode,
                    self.sess.connection_id,
                    &self.tbl_info,
                    &idx_info,
                    &mut self.idx_vals,
                ) {
                    Ok(b) => b,
                    Err(err) => {
                        if ErrNotExist.equal(&err) {
                            return Ok(());
                        }
                        return Err(err);
                    }
                };
                self.handle = Some(Handle::Common(Arc::new(new_common_handle(handle_bytes)?)));
            } else {
                self.idx_key = match encode_unique_index_key(
                    &ctx.stmt,
                    self.sess.sql_mode,
                    self.sess.connection_id,
                    &self.tbl_info,
                    &idx_info,
                    &mut self.idx_vals,
                    tbl_id,
                ) {
                    Ok(k) => Some(Key(k)),
                    Err(err) => {
                        if !ErrNotExist.equal(&err) {
                            return Err(err);
                        }
                        None
                    }
                };

                // lockNonExistIdxKey indicates the key will be locked regardless of its existence.
                let lock_non_exist_idx_key = !self.sess.pessimistic_read_consistency;
                // Non-exist keys are also locked if the isolation level is not read consistency,
                // lock it before read here, then it's able to read from pessimistic lock cache.
                let idx_key = self.idx_key.clone();
                if lock_non_exist_idx_key {
                    self.lock_key_if_needed(idx_key.as_ref())?;
                    self.handle_val = match self.get(idx_key.as_ref()).await {
                        Ok(v) => v,
                        Err(err) => {
                            if !ErrNotExist.equal(&err) {
                                return Err(err);
                            }
                            Vec::new()
                        }
                    };
                } else if self.lock {
                    self.handle_val = self
                        .lock_key_if_exists(idx_key.as_ref())?
                        .unwrap_or_default();
                } else {
                    self.handle_val = match self.get(idx_key.as_ref()).await {
                        Ok(v) => v,
                        Err(err) => {
                            if !ErrNotExist.equal(&err) {
                                return Err(err);
                            }
                            Vec::new()
                        }
                    };
                }

                if self.handle_val.is_empty() {
                    return Ok(());
                }

                let iv = tablecodec::decode_handle_in_index_value(&self.handle_val)?;
                self.handle = iv;

                // The "pointGetRepeatableReadTest" failpoints are not ported.
                if idx_info.global {
                    // Go's nil PartitionID decodes to an error, as an empty slice does.
                    let (_, pid) = tidb_codec::decode_int(
                        tablecodec::split_index_value(&self.handle_val)
                            .partition_id
                            .unwrap_or(&[]),
                    )?;
                    tbl_id = pid;
                    let Some(pi) = self.tbl_info.get_partition_info() else {
                        // Go dereferences the nil partition info.
                        panic!("a global index of a table without partitions");
                    };
                    if !match_partition_names(tbl_id, &self.partition_names, pi) {
                        return Ok(());
                    }
                    for id in pi.ids_in_ddl_to_ignore().unwrap_or_default() {
                        if id == pid {
                            return Ok(());
                        }
                    }
                }
            }
        }

        let handle = self
            .handle
            .clone()
            .expect("a point get reads a handle (from the plan or the index)");
        let key = tablecodec::encode_row_key_with_handle(tbl_id, &handle);
        let val = self.get_and_lock(&key).await?.unwrap_or_default();
        if val.is_empty() {
            if let Some(idx_info) = &self.idx_info
                && !is_common_handle_read(&self.tbl_info, idx_info)
                && !ctx.stmt.weak_consistency
            {
                // Go's HandleEncode / IndexEncode / Storage feed the MVCC dump only (see
                // crate::consistency).
                return Err(Reporter {
                    tbl: &self.tbl_info,
                    idx: idx_info,
                    enable_redact_log: self.sess.enable_redact_log.clone(),
                }
                .report_lookup_inconsistent(
                    1,
                    0,
                    std::slice::from_ref(&handle),
                    std::slice::from_ref(&handle),
                    1,
                ));
            }
            return Ok(());
        }

        let schema = self.base.base.meta.schema();
        let expr_ctx = Arc::clone(&self.sess.expr_ctx);
        decode_row_val_to_chunk(
            &*expr_ctx,
            &self.sess.location,
            &schema,
            &self.tbl_info,
            &handle,
            &val,
            req,
            self.row_decoder.as_ref().expect("Init built the decoder"),
        )?;

        // Go: `sctx.GetSessionVars().TimeZone`; `Location()` here (see fill_row_checksum).
        fill_row_checksum(
            &*expr_ctx,
            &self.sess.location,
            0,
            1,
            &schema,
            &self.tbl_info,
            &[&val],
            &[&handle],
            req,
            Vec::new(),
        )?;

        fill_virtual_column_value(
            &self.virtual_column_ret_field_types,
            &self.virtual_column_index,
            &schema.columns,
            &self.columns,
            &*expr_ctx,
            req,
        )?;
        Ok(())
    }

    // glue: M-IFACE (type assertion to *PointGetExecutor)
    fn as_point_get(&mut self) -> Option<&mut dyn glue::PointGetExecutor> {
        Some(self)
    }
}

impl glue::PointGetExecutor for PointGetExecutor {
    // glue: Go method PointGetExecutor.Init through the type assertion
    fn init(&mut self, sctx: &mut dyn SessionContext, plan: &Plan) -> Result<(), Error> {
        let Some(p) = plan.as_any().downcast_ref::<PointGetPlan>() else {
            return Err(Error::plain(
                "a cached point get executor with a plan that is not a *core.PointGetPlan"
                    .to_owned(),
            ));
        };
        PointGetExecutor::init(self, sctx, p);
        Ok(())
    }
    // glue: Go field PointGetExecutor.partitionDefIdx
    fn is_partitioned(&self) -> bool {
        self.partition_def_idx.is_some()
    }
    // glue: N3 (see the trait)
    fn release_stmt_state(&mut self) {
        self.base.base.stats.release_stmt_ctx();
        self.index_usage_reporter = None;
        self.stats = None;
    }
}

// go: shouldFillRowChecksum L1
fn should_fill_row_checksum(schema: &Schema) -> (usize, bool) {
    for (idx, col) in schema.columns.iter().enumerate() {
        if col.id == ExtraRowChecksumID as i64 {
            return (idx, true);
        }
    }
    (0, false)
}

/// The row checksum column (`_tidb_row_checksum`) of rows `start..end`.
///
/// `sctx` is what Go reads from the session: its expression context and time zone. The
/// field types of the decode are the table columns', borrowed (Go `&col.FieldType`).
///
/// Time zone: Go passes `SessionVars.TimeZone`, which may be nil (`DecodeRowWithMapNew`
/// then leaves TIMESTAMP values in UTC, and `ConvertTimeZone(UTC, nil)` of a non-zero one
/// panics in `Time.In`); the caller here passes `Location()` (the system zone when nil).
/// The two are the same once the session has loaded `time_zone`, which a session always
/// does before it runs a statement (D12.1 spec §4 item 3). TIMESTAMP values are decoded
/// into that zone and converted back to UTC for the checksum (`appendDatumForChecksum`),
/// so a consistent zone gives the UTC-based checksum.
// go: fillRowChecksum L1 [M-CTX]
#[allow(clippy::too_many_arguments)]
pub fn fill_row_checksum(
    ectx: &dyn BuildContext,
    tz: &Arc<Location>,
    start: usize,
    end: usize,
    schema: &Schema,
    tbl_info: &TableInfo,
    values: &[&[u8]],
    handles: &[&Handle],
    req: &mut Chunk,
    buf: Vec<u8>,
) -> Result<(), Error> {
    let (checksum_column_index, ok) = should_fill_row_checksum(schema);
    if !ok {
        return Ok(());
    }

    let tbl_columns = tbl_info.columns.as_deref().unwrap_or(&[]);
    let mut handle_col_ids: Vec<i64> = Vec::new();
    if tbl_info.pk_is_handle {
        let col_info = tbl_info
            .get_pk_col_info()
            .expect("PKIsHandle has a PK column");
        handle_col_ids = vec![col_info.id];
    } else if tbl_info.is_common_handle {
        let pk_idx = find_primary_index(tbl_info).expect("a clustered table has a primary index");
        for col in pk_idx.columns.as_deref().unwrap_or(&[]) {
            let col_info = &tbl_columns[col.offset as usize];
            handle_col_ids.push(col_info.id);
        }
    }

    let mut column_ft: HashMap<i64, &FieldType> = HashMap::new();
    for col in tbl_columns {
        column_ft.insert(col.id, &col.field_type);
    }
    let ft = [Arc::clone(&schema.columns[checksum_column_index].ret_type)];
    let mut checksum_cols = new_chunk_with_capacity(&ft, req.capacity());
    let mut buf = buf;
    for i in start..end {
        let (handle, val) = (handles[i], values[i]);
        if !is_new_format(val) {
            checksum_cols.append_null(0);
            continue;
        }
        let datums = tablecodec::decode_row_with_map_new(val, &column_ft, Some(tz), None)?;
        let (mut datums, err) = tablecodec::decode_handle_to_datum_map(
            Some(handle),
            &handle_col_ids,
            &column_ft,
            Some(tz),
            Some(datums),
        );
        err?;
        for col in tbl_columns {
            // cannot found from the datums, which means the data is not stored, this
            // may happen after `add column` executed, filling with the default value.
            if let std::collections::hash_map::Entry::Vacant(e) = datums.entry(col.id) {
                let col_info = get_col_info_by_id(tbl_info, col.id).expect("a column of the table");
                let d = get_col_origin_default_value(ectx, col_info)?;
                e.insert(d);
            }
        }

        let col_data: Vec<ColData<'_>> = tbl_columns
            .iter()
            .map(|col| ColData {
                column_info: col,
                datum: &datums[&col.id],
            })
            .collect();
        let mut row = RowData {
            cols: col_data,
            data: std::mem::take(&mut buf),
        };
        if !row
            .cols
            .is_sorted_by(|a, b| a.column_info.id <= b.column_info.id)
        {
            row.cols.sort_by_key(|c| c.column_info.id);
        }
        let checksum = row.checksum(Some(tz))?;
        buf = std::mem::take(&mut row.data);
        checksum_cols.append_string(0, checksum.to_string().as_bytes());
    }
    req.set_col(checksum_column_index, checksum_cols.column_arc(0).clone());
    Ok(())
}

/// DecodeRowValToChunk decodes row value into chunk checking row format used.
///
/// `sctx` is what Go reads from the session: its expression context (the defaults of the
/// old row format) and its time zone.
// go: DecodeRowValToChunk L1 [M-CTX]
#[allow(clippy::too_many_arguments)]
pub fn decode_row_val_to_chunk(
    ectx: &dyn BuildContext,
    loc: &Arc<Location>,
    schema: &Schema,
    tbl_info: &TableInfo,
    handle: &Handle,
    row_val: &[u8],
    chk: &mut Chunk,
    rd: &ChunkDecoder,
) -> Result<(), Error> {
    if is_new_format(row_val) {
        return rd.decode_to_chunk(row_val, 0, Some(handle), chk);
    }
    decode_old_row_val_to_chunk(ectx, loc, schema, tbl_info, handle, row_val, chk)
}

// go: decodeOldRowValToChunk L1 [M-CTX]
fn decode_old_row_val_to_chunk(
    ectx: &dyn BuildContext,
    loc: &Arc<Location>,
    schema: &Schema,
    tbl_info: &TableInfo,
    handle: &Handle,
    row_val: &[u8],
    chk: &mut Chunk,
) -> Result<(), Error> {
    let pk_cols = try_get_common_pk_column_ids(tbl_info).unwrap_or_default();
    let prefix_col_ids = primary_prefix_column_ids(tbl_info).unwrap_or_default();
    let mut col_id2_cut_pos: HashMap<i64, usize> = HashMap::with_capacity(schema.len());
    for col in &schema.columns {
        if !col_id2_cut_pos.contains_key(&col.id) {
            let n = col_id2_cut_pos.len();
            col_id2_cut_pos.insert(col.id, n);
        }
    }
    let mut cut_vals = tablecodec::cut_row_new(row_val, &col_id2_cut_pos)?;
    if cut_vals.is_empty() {
        cut_vals = vec![&[][..]; col_id2_cut_pos.len()];
    }
    let mut decoder = new_decoder(chk, Arc::clone(loc));
    for (i, col) in schema.columns.iter().enumerate() {
        // fill the virtual column value after row calculation
        if col.virtual_expr.is_some() {
            decoder.chunk().append_null(i);
            continue;
        }
        let ok = try_decode_from_handle(
            tbl_info,
            i,
            col,
            handle,
            &mut decoder,
            &pk_cols,
            &prefix_col_ids,
        )?;
        if ok {
            continue;
        }
        let cut_pos = col_id2_cut_pos[&col.id];
        if cut_vals[cut_pos].is_empty() {
            // Go dereferences the nil column info of an ID the table does not have (a panic).
            let col_info = get_col_info_by_id(tbl_info, col.id)
                .expect("decodeOldRowValToChunk: column info of a schema column");
            let d = get_col_origin_default_value(ectx, col_info)?;
            decoder.chunk().append_datum(i, &d);
            continue;
        }
        decoder.decode_one(cut_vals[cut_pos], i, &col.ret_type)?;
    }
    Ok(())
}

// go: tryDecodeFromHandle L1
#[allow(clippy::too_many_arguments)]
fn try_decode_from_handle(
    tbl_info: &TableInfo,
    schema_col_idx: usize,
    col: &Column,
    handle: &Handle,
    decoder: &mut Decoder<'_>,
    pk_cols: &[i64],
    prefix_col_ids: &[i64],
) -> Result<bool, Error> {
    if tbl_info.pk_is_handle && mysql::has_pri_key_flag(col.ret_type.get_flag()) {
        decoder
            .chunk()
            .append_int64(schema_col_idx, handle.int_value());
        return Ok(true);
    }
    if col.id == ExtraHandleID as i64 {
        decoder
            .chunk()
            .append_int64(schema_col_idx, handle.int_value());
        return Ok(true);
    }
    if need_restored_data(&col.ret_type) {
        return Ok(false);
    }
    // Try to decode common handle.
    if mysql::has_pri_key_flag(col.ret_type.get_flag()) {
        for (i, &hid) in pk_cols.iter().enumerate() {
            if col.id == hid && not_pk_prefix_col(hid, prefix_col_ids) {
                decoder.decode_one(
                    &handle.encoded_col(i as isize),
                    schema_col_idx,
                    &col.ret_type,
                )?;
                return Ok(true);
            }
        }
    }
    Ok(false)
}

// go: notPKPrefixCol L1
fn not_pk_prefix_col(col_id: i64, prefix_col_ids: &[i64]) -> bool {
    for &p_col in prefix_col_ids {
        if p_col == col_id {
            return false;
        }
    }
    true
}

// go: getColInfoByID L1
fn get_col_info_by_id(tbl: &TableInfo, col_id: i64) -> Option<&Arc<ColumnInfo>> {
    tbl.columns.iter().flatten().find(|col| col.id == col_id)
}

/// The runtime stats of a point get: its snapshot's (client-go `SnapshotRuntimeStats`; the
/// stand-in [`SnapshotRuntimeStats`] of client-rust's missing stats).
#[derive(Clone, Debug, Default)]
pub struct RuntimeStatsWithSnapshot {
    pub snapshot_runtime_stats: Option<SnapshotRuntimeStats>,
}

impl RuntimeStats for RuntimeStatsWithSnapshot {
    // go: runtimeStatsWithSnapshot.String L1
    fn string(&self) -> String {
        if let Some(s) = &self.snapshot_runtime_stats {
            return s.string();
        }
        String::new()
    }

    /// Clone implements the RuntimeStats interface.
    // go: runtimeStatsWithSnapshot.Clone L1
    fn clone_stats(&self) -> Box<dyn RuntimeStats> {
        let mut new_rs = RuntimeStatsWithSnapshot::default();
        if let Some(s) = &self.snapshot_runtime_stats {
            new_rs.snapshot_runtime_stats = Some(s.clone());
        }
        Box::new(new_rs)
    }

    /// Merge implements the RuntimeStats interface.
    // go: runtimeStatsWithSnapshot.Merge L1 [M-IFACE]
    fn merge(&mut self, other: &dyn RuntimeStats) {
        let Some(tmp) = other.as_any().downcast_ref::<RuntimeStatsWithSnapshot>() else {
            return;
        };
        if let Some(t) = &tmp.snapshot_runtime_stats {
            match &mut self.snapshot_runtime_stats {
                None => {
                    self.snapshot_runtime_stats = Some(t.clone());
                }
                Some(s) => s.merge(t),
            }
        }
    }

    /// Tp implements the RuntimeStats interface.
    // go: runtimeStatsWithSnapshot.Tp L1
    fn tp(&self) -> i64 {
        TpRuntimeStatsWithSnapshot
    }

    // glue: type assertion support
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests;
