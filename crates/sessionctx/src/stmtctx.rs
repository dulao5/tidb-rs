//! go-file: pkg/sessionctx/stmtctx/stmtctx.go
//!
//! `StatementContext` (block D00.1, part 1): the per-statement state the read path uses:
//! the type and error contexts, the warning handlers, the statement kind flags, the row
//! counters, the plan cache tracker and range fallback handler, the statement cache, the
//! push-down flags and the stale TSO provider.
//!
//! Not in part 1 (registered as govec exemptions with the block they wait for): the SQL
//! and plan digests (`parser.Digest`), the resource group tagger, the memory / disk trackers
//! of the statement, `SyncExecDetails` and the runtime stats (D14.0), the used-stats info
//! (statistics, D11), the logical-plan build state (model.TableInfo keyed maps, intset),
//! the DistSQL context cache and the statement label (ast).
//!
//! Mapping:
//! - In Go the statement context is itself the `WarnAppender` of its `types.Context`,
//!   `errctx.Context` and plan cache tracker (`sc` is passed to them). Here that role is a
//!   shared [`StmtWarnSink`]: it forwards to the current `WarnHandler`, so replacing the
//!   handler (as `Reset` may) is seen by the contexts, as with Go's pointer.
//! - `mu`, `stmtCache` and `StaleTSOProvider` are mutexes, as in Go. `Reset` takes
//!   `&mut self`, so their `TryLock` always succeeds (L2: Go's false result needs another
//!   goroutine holding a lock while the context is reset, which a `&mut` borrow excludes).
//! - `any` values are `Arc<dyn Any + Send + Sync>`.

// Constants keep their Go names (mapping rule M-GONAME).
#![allow(non_upper_case_globals)]

use std::any::Any;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, RwLock};
use std::time::Duration;

use tidb_base::contextutil::{
    PlanCacheTracker, RangeFallbackHandler, SQLWarn, WarnAppender, WarnHandlerExtRef,
    gen_context_id, new_plan_cache_tracker, new_range_fallback_handler, new_static_warn_handler,
};
use tidb_base::dbterror::plannererrors;
use tidb_base::gostd::time::{Location, utc};
use tidb_base::mysql::PriorityEnum;
use tidb_base::{Arg, Error};
use tidb_types::context::{
    Context as TypeContext, DefaultStmtFlags, DefaultStmtNoWarningContext, Flags,
};

use crate::errctx;

static TASK_ID_ALLOC: AtomicU64 = AtomicU64::new(0);

/// AllocateTaskID allocates a new unique ID for a statement execution
// go: AllocateTaskID L1
pub fn allocate_task_id() -> u64 {
    TASK_ID_ALLOC.fetch_add(1, Ordering::SeqCst) + 1
}

/// ReferenceCount indicates the reference count of StmtCtx.
#[derive(Debug, Default)]
pub struct ReferenceCount(pub AtomicI32);

/// ReferenceCountIsFrozen indicates the current StmtCtx is resetting, it'll refuse all the
/// access from other sessions.
pub const ReferenceCountIsFrozen: i32 = -1;
/// ReferenceCountNoReference indicates the current StmtCtx is not accessed by other
/// sessions.
pub const ReferenceCountNoReference: i32 = 0;

impl ReferenceCount {
    /// TryIncrease tries to increase the reference count. There is a small chance that
    /// TryIncrease returns true while TryFreeze and UnFreeze are invoked successfully during
    /// the execution of TryIncrease.
    // go: ReferenceCount.TryIncrease L1
    pub fn try_increase(&self) -> bool {
        let mut ref_cnt = self.0.load(Ordering::SeqCst);
        while ref_cnt != ReferenceCountIsFrozen
            && self
                .0
                .compare_exchange(
                    ref_cnt,
                    ref_cnt.wrapping_add(1),
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                )
                .is_err()
        {
            ref_cnt = self.0.load(Ordering::SeqCst);
        }
        ref_cnt != ReferenceCountIsFrozen
    }

    /// Decrease decreases the reference count.
    // go: ReferenceCount.Decrease L1
    pub fn decrease(&self) {
        let mut ref_cnt = self.0.load(Ordering::SeqCst);
        while self
            .0
            .compare_exchange(
                ref_cnt,
                ref_cnt.wrapping_sub(1),
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_err()
        {
            ref_cnt = self.0.load(Ordering::SeqCst);
        }
    }

    /// TryFreeze tries to freeze the StmtCtx to frozen before resetting the old StmtCtx.
    // go: ReferenceCount.TryFreeze L1
    pub fn try_freeze(&self) -> bool {
        self.0.load(Ordering::SeqCst) == ReferenceCountNoReference
            && self
                .0
                .compare_exchange(
                    ReferenceCountNoReference,
                    ReferenceCountIsFrozen,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                )
                .is_ok()
    }

    /// UnFreeze unfreeze the frozen StmtCtx thus the other session can access this StmtCtx.
    // go: ReferenceCount.UnFreeze L1
    pub fn un_freeze(&self) {
        self.0.store(ReferenceCountNoReference, Ordering::SeqCst);
    }
}

/// ReservedRowIDAlloc is used to reserve autoID for the auto_increment column.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReservedRowIDAlloc {
    base: i64,
    max: i64,
}

impl ReservedRowIDAlloc {
    /// Reset resets the base and max of reserved rowIDs.
    // go: ReservedRowIDAlloc.Reset L1
    pub fn reset(&mut self, base: i64, maxv: i64) {
        self.base = base;
        self.max = maxv;
    }

    /// Consume consumes a reserved rowID. If the second return value is false, it means the
    /// reserved rowID is exhausted.
    // go: ReservedRowIDAlloc.Consume L1
    pub fn consume(&mut self) -> (i64, bool) {
        if self.base < self.max {
            self.base += 1;
            return (self.base, true);
        }
        (0, false)
    }

    /// Exhausted returns whether the reserved rowID is exhausted.
    // go: ReservedRowIDAlloc.Exhausted L1
    pub fn exhausted(&self) -> bool {
        self.base >= self.max
    }
}

/// `stmtCtxMu`: the row counters and the message, under `StatementContext.mu`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct StmtCtxMu {
    affected_rows: u64,
    found_rows: u64,
    // These variables serve a similar purpose to those in MySQL's `COPY_INFO`, they are
    // used to count rows for INSERT/REPLACE/UPDATE queries.
    records: u64,
    deleted: u64,
    updated: u64,
    copied: u64,
    touched: u64,
    message: String,
}

impl StmtCtxMu {
    // go: stmtCtxMu.reset L1
    fn reset(&mut self) {
        self.affected_rows = 0;
        self.copied = 0;
        self.deleted = 0;
        self.found_rows = 0;
        self.message.clear();
        self.records = 0;
        self.touched = 0;
        self.updated = 0;
    }
}

/// The evaluator of a [`StaleTSOProvider`].
pub type StaleTSOEval = Box<dyn FnMut() -> Result<u64, Error> + Send>;

/// `staleTSOProvider`: the lazily evaluated stale read TSO of the statement.
#[derive(Default)]
struct StaleTSOProvider {
    value: Option<u64>,
    eval: Option<StaleTSOEval>,
}

impl StaleTSOProvider {
    // go: staleTSOProvider.reset L1
    fn reset(&mut self) {
        self.value = None;
        self.eval = None;
    }
}

/// A value of the statement cache (Go `any`).
pub type StmtCacheValue = Arc<dyn Any + Send + Sync>;

/// `stmtCache`: values computed once per statement (`now()`, safe TS, external TS).
#[derive(Default)]
struct StmtCache {
    /// `nil` until the first store, as in Go.
    data: Option<HashMap<StmtCacheKey, StmtCacheValue>>,
}

impl StmtCache {
    // go: stmtCache.reset L1
    fn reset(&mut self) {
        self.data = None;
    }
}

/// StmtCacheKey represents the key type in the StmtCache.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct StmtCacheKey(pub i64);

/// StmtNowTsCacheKey is a variable for now/current_timestamp calculation/cache of one stmt.
pub const StmtNowTsCacheKey: StmtCacheKey = StmtCacheKey(0);
/// StmtSafeTSCacheKey is a variable for safeTS calculation/cache of one stmt.
pub const StmtSafeTSCacheKey: StmtCacheKey = StmtCacheKey(1);
/// StmtExternalTSCacheKey is a variable for externalTS calculation/cache of one stmt.
pub const StmtExternalTSCacheKey: StmtCacheKey = StmtCacheKey(2);

/// TableEntry presents table in db.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TableEntry {
    pub db: String,
    pub table: String,
}

/// The `WarnAppender` role of the statement context: it forwards to the current
/// `WarnHandler` (Go passes `sc` itself to the type / error contexts and the trackers).
pub struct StmtWarnSink {
    handler: RwLock<WarnHandlerExtRef>,
}

impl StmtWarnSink {
    // glue: reads sc.WarnHandler (an interface field)
    fn get(&self) -> WarnHandlerExtRef {
        Arc::clone(&self.handler.read().unwrap_or_else(|e| e.into_inner()))
    }

    // glue: assigns sc.WarnHandler
    fn set(&self, h: WarnHandlerExtRef) {
        *self.handler.write().unwrap_or_else(|e| e.into_inner()) = h;
    }
}

impl WarnAppender for StmtWarnSink {
    // glue: StatementContext.AppendWarning through the WarnAppender interface
    fn append_warning(&self, err: Error) {
        self.get().append_warning(err);
    }

    // glue: StatementContext.AppendNote through the WarnAppender interface
    fn append_note(&self, err: Error) {
        self.get().append_note(err);
    }
}

/// DefaultStmtErrLevels is the default error levels for statement
pub const DefaultStmtErrLevels: errctx::LevelMap = {
    let mut l = [errctx::LevelError; 7];
    l[errctx::ErrGroupDividedByZero.0 as usize] = errctx::LevelWarn;
    l
};

/// StatementContext contains variables for a statement. It should be reset before executing
/// a statement.
pub struct StatementContext {
    ctx_id: u64,
    type_ctx: TypeContext,
    err_ctx: errctx::Context,
    ranger_ctx_cache: OnceLock<Arc<dyn Any + Send + Sync>>,
    build_pb_ctx_cache: OnceLock<Arc<dyn Any + Send + Sync>>,

    /// IsDDLJobInQueue is used to mark whether the DDL job is put into the queue. If
    /// IsDDLJobInQueue is true, it means the DDL job is in the queue of storage, and it can
    /// be handled by the DDL worker.
    pub is_ddl_job_in_queue: bool,
    pub ddl_job_id: i64,
    pub in_insert_stmt: bool,
    pub in_update_stmt: bool,
    pub in_delete_stmt: bool,
    pub in_select_stmt: bool,
    pub in_load_data_stmt: bool,
    pub in_explain_stmt: bool,
    pub in_explain_analyze_stmt: bool,
    pub explain_format: String,
    pub in_create_or_alter_stmt: bool,
    pub in_set_session_states_stmt: bool,
    pub in_prepared_plan_building: bool,
    pub in_show_warning: bool,

    /// `contextutil.PlanCacheTracker` (embedded in Go).
    pub plan_cache_tracker: Arc<PlanCacheTracker>,
    /// `contextutil.RangeFallbackHandler` (embedded in Go).
    pub range_fallback_handler: RangeFallbackHandler,

    /// IgnoreExplainIDSuffix indicates whether to ignore the explain ID suffix.
    pub ignore_explain_id_suffix: bool,
    /// IsStaleness means whether read stale data now.
    pub is_staleness: bool,
    pub in_restricted_sql: bool,
    pub view_depth: i32,

    mu: Mutex<StmtCtxMu>,
    /// WarnHandler stores warnings and errors (the current handler behind [`StmtWarnSink`]).
    warn_sink: Arc<StmtWarnSink>,
    /// ExtraWarnHandler record the extra warnings and are only used by the slow log only
    /// now.
    pub extra_warn_handler: WarnHandlerExtRef,

    /// PrevAffectedRows is the affected-rows value(DDL is 0, DML is the number of affected
    /// rows).
    pub prev_affected_rows: i64,
    /// PrevLastInsertID is the last insert ID of previous statement.
    pub prev_last_insert_id: u64,
    /// LastInsertID is the auto-generated ID used in the current statement.
    pub last_insert_id: u64,
    /// InsertID is the given insert ID of an auto_increment column.
    pub insert_id: u64,
    /// ReservedRowIDAlloc is used to alloc auto ID from the reserved IDs.
    pub reserved_row_id_alloc: ReservedRowIDAlloc,

    pub priority: PriorityEnum,
    pub not_fill_cache: bool,
    /// ResourceGroupName is the name of the resource group of the statement.
    pub resource_group_name: String,
    pub is_tikv: AtomicBool,
    pub is_tiflash: AtomicBool,
    pub table_ids: Vec<i64>,
    pub index_names: Vec<String>,
    pub stmt_type: String,
    pub original_sql: String,
    /// BindSQL used to construct the key for plan cache. It records the binding used by the
    /// stmt. If the binding is not used by the stmt, the value is empty
    pub bind_sql: String,
    /// The several fields below are mainly for some diagnostic features, like stmt
    /// summary and slow query. We cache the values here to avoid calculating them multiple
    /// times. Note: Constructing and setting these fields is a part of the execution.
    pub exec_retry_count: u64,
    pub exec_success: bool,

    plan_normalized: String,
    encoded_plan: String,
    plan_hint: String,
    plan_hint_set: bool,
    binary_plan: String,
    /// To record whether a index force or not.
    index_force: bool,
    /// Fields below are for the plan; they are `any` in Go (the plan types are not here).
    flat_plan: Option<Arc<dyn Any + Send + Sync>>,
    plan: Option<Arc<dyn Any + Send + Sync>>,

    pub tables: Vec<TableEntry>,
    /// LockWaitStartTime stores the pessimistic lock wait start time
    lock_wait_start_time: AtomicI64,
    pub pessimistic_lock_waited: AtomicI32,
    pub lock_keys_duration: AtomicI64,
    pub lock_keys_count: AtomicI32,
    /// table IDs need to be locked, empty for lock all tables
    pub lock_table_ids: HashSet<i64>,
    /// unique ID for an execution of a statement
    pub task_id: u64,
    /// counter for
    pub task_map_bak_ts: u64,

    stmt_cache: Mutex<StmtCache>,

    /// SetVarHintRestore record the variables that need to be restored after the statement.
    pub set_var_hint_restore: Option<HashMap<String, String>>,
    /// ReadFromTableCache indicates whether the statement is read from table cache.
    pub read_from_table_cache: bool,
    /// InVerboseExplain indicates the statement is "explain format='verbose' ...".
    pub in_verbose_explain: bool,
    /// EnableOptimizeTrace indicates whether enable optimizer trace by 'trace plan statement'
    pub enable_optimize_trace: bool,
    /// EnableOptimizerCETrace indicate if cardinality estimation internal process needs to be
    /// traced.
    pub enable_optimizer_ce_trace: bool,
    /// EnableOptimizerDebugTrace indicate if optimizer debug trace should be enabled.
    pub enable_optimizer_debug_trace: bool,
    /// WaitLockLeaseTime is the duration of cached table read lease expiration time.
    pub wait_lock_lease_time: Duration,
    /// WeakConsistency is true when read consistency is weak and in a read statement and
    /// not in a transaction.
    pub weak_consistency: bool,
    /// StatsLoad.Timeout: the timeout of the synchronous stats loading.
    pub stats_load_timeout: Duration,
    /// SysdateIsNow indicates whether sysdate() is an alias of now() in this statement
    pub sysdate_is_now: bool,
    /// RCCheckTS indicates the current read-consistency read select statement will use
    /// `RCCheckTS` path.
    pub rc_check_ts: bool,
    /// IsSQLRegistered uses to indicate whether the SQL has been registered for TopSQL.
    pub is_sql_registered: AtomicBool,
    /// IsSQLAndPlanRegistered uses to indicate whether the SQL and plan has been registered
    /// for TopSQL.
    pub is_sql_and_plan_registered: AtomicBool,
    /// IsReadOnly uses to indicate whether the SQL is read-only.
    pub is_read_only: bool,
    /// IsSyncStatsFailed indicates whether any failure happened during sync stats
    pub is_sync_stats_failed: bool,
    /// UseDynamicPruneMode indicates whether use UseDynamicPruneMode in query stmt
    pub use_dynamic_prune_mode: bool,
    pub alternative_logical_plan_decorrelated_apply: bool,
    pub alternative_logical_plan_same_order_index_join: bool,
    pub alternative_logical_plan_prefer_correlate: bool,
    /// IsExplainAnalyzeDML is true if the statement is "explain analyze DML executors",
    /// before responding the explain results to the client, the transaction should be
    /// committed for optimistic transactions and rolled back for pessimistic transactions.
    pub is_explain_analyze_dml: bool,
    /// InHandleForeignKeyTrigger indicates currently are handling foreign key trigger.
    pub in_handle_foreign_key_trigger: bool,
    /// ForeignKeyTriggerCtx.SavepointName: the savepoint of the foreign key trigger.
    pub foreign_key_trigger_savepoint_name: String,
    /// ForeignKeyTriggerCtx.HasFKCascades: whether the statement has foreign key cascades.
    pub foreign_key_trigger_has_fk_cascades: bool,
    /// MPPQueryInfo: the query ID, TS, allocated task and gather IDs of an MPP query.
    pub mpp_query_id: AtomicU64,
    pub mpp_query_ts: AtomicU64,
    pub mpp_allocated_task_id: AtomicI64,
    pub mpp_allocated_gather_id: AtomicU64,
    /// TableStats stores the visited runtime table stats by table id during query
    pub table_stats: HashMap<i64, Arc<dyn Any + Send + Sync>>,
    /// useChunkAlloc indicates whether statement use chunk alloc
    use_chunk_alloc: bool,
    /// TiFlashEngineRemovedDueToStrictSQLMode indicates whether tiflash engine is removed
    /// due to strict sql mode.
    pub tiflash_engine_removed_due_to_strict_sql_mode: bool,
    /// StaleTSOProvider is used to provide stale timestamp oracle for read-only queries.
    stale_tso_provider: Mutex<StaleTSOProvider>,
    /// RelatedTableIDs records all table ids processing in this stmt.
    pub related_table_ids: HashSet<i64>,
    /// ForShareLockEnabledByNoop indicates whether the current statement contains `for
    /// share` clause and the `for share` execution is enabled by
    /// `tidb_enable_noop_functions`, no locks should be acquired in this case.
    pub for_share_lock_enabled_by_noop: bool,
}

impl std::fmt::Debug for StatementContext {
    // glue: Debug for a struct holding trait objects
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StatementContext")
            .field("ctx_id", &self.ctx_id)
            .field("type_ctx", &self.type_ctx)
            .field("err_ctx", &self.err_ctx)
            .field("stmt_type", &self.stmt_type)
            .finish_non_exhaustive()
    }
}

// glue: sync.Mutex.Lock (a panic while held does not poison the state, as in Go)
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// NewStmtCtx creates a new statement context
// go: NewStmtCtx L1
pub fn new_stmt_ctx() -> StatementContext {
    new_stmt_ctx_with_time_zone(utc())
}

/// NewStmtCtxWithTimeZone creates a new StatementContext with the given timezone
// go: NewStmtCtxWithTimeZone L1
pub fn new_stmt_ctx_with_time_zone(tz: Arc<Location>) -> StatementContext {
    let warn_sink = Arc::new(StmtWarnSink {
        handler: RwLock::new(Arc::new(new_static_warn_handler(0))),
    });
    let appender: tidb_types::context::WarnHandler = warn_sink.clone();
    let type_ctx = tidb_types::context::new_context(DefaultStmtFlags, tz, appender.clone());
    let err_ctx = new_err_ctx(&type_ctx, DefaultStmtErrLevels, appender.clone());
    let plan_cache_tracker = Arc::new(new_plan_cache_tracker(Some(appender.clone())));
    let range_fallback_handler =
        new_range_fallback_handler(Arc::clone(&plan_cache_tracker), appender);
    StatementContext {
        ctx_id: gen_context_id(),
        type_ctx,
        err_ctx,
        ranger_ctx_cache: OnceLock::new(),
        build_pb_ctx_cache: OnceLock::new(),
        is_ddl_job_in_queue: false,
        ddl_job_id: 0,
        in_insert_stmt: false,
        in_update_stmt: false,
        in_delete_stmt: false,
        in_select_stmt: false,
        in_load_data_stmt: false,
        in_explain_stmt: false,
        in_explain_analyze_stmt: false,
        explain_format: String::new(),
        in_create_or_alter_stmt: false,
        in_set_session_states_stmt: false,
        in_prepared_plan_building: false,
        in_show_warning: false,
        plan_cache_tracker,
        range_fallback_handler,
        ignore_explain_id_suffix: false,
        is_staleness: false,
        in_restricted_sql: false,
        view_depth: 0,
        mu: Mutex::new(StmtCtxMu::default()),
        warn_sink,
        extra_warn_handler: Arc::new(new_static_warn_handler(0)),
        prev_affected_rows: 0,
        prev_last_insert_id: 0,
        last_insert_id: 0,
        insert_id: 0,
        reserved_row_id_alloc: ReservedRowIDAlloc::default(),
        priority: PriorityEnum::default(),
        not_fill_cache: false,
        resource_group_name: String::new(),
        is_tikv: AtomicBool::new(false),
        is_tiflash: AtomicBool::new(false),
        table_ids: Vec::new(),
        index_names: Vec::new(),
        stmt_type: String::new(),
        original_sql: String::new(),
        bind_sql: String::new(),
        exec_retry_count: 0,
        exec_success: false,
        plan_normalized: String::new(),
        encoded_plan: String::new(),
        plan_hint: String::new(),
        plan_hint_set: false,
        binary_plan: String::new(),
        index_force: false,
        flat_plan: None,
        plan: None,
        tables: Vec::new(),
        lock_wait_start_time: AtomicI64::new(0),
        pessimistic_lock_waited: AtomicI32::new(0),
        lock_keys_duration: AtomicI64::new(0),
        lock_keys_count: AtomicI32::new(0),
        lock_table_ids: HashSet::new(),
        task_id: 0,
        task_map_bak_ts: 0,
        stmt_cache: Mutex::new(StmtCache::default()),
        set_var_hint_restore: None,
        read_from_table_cache: false,
        in_verbose_explain: false,
        enable_optimize_trace: false,
        enable_optimizer_ce_trace: false,
        enable_optimizer_debug_trace: false,
        wait_lock_lease_time: Duration::ZERO,
        weak_consistency: false,
        stats_load_timeout: Duration::ZERO,
        sysdate_is_now: false,
        rc_check_ts: false,
        is_sql_registered: AtomicBool::new(false),
        is_sql_and_plan_registered: AtomicBool::new(false),
        is_read_only: false,
        is_sync_stats_failed: false,
        use_dynamic_prune_mode: false,
        alternative_logical_plan_decorrelated_apply: false,
        alternative_logical_plan_same_order_index_join: false,
        alternative_logical_plan_prefer_correlate: false,
        is_explain_analyze_dml: false,
        in_handle_foreign_key_trigger: false,
        foreign_key_trigger_savepoint_name: String::new(),
        foreign_key_trigger_has_fk_cascades: false,
        mpp_query_id: AtomicU64::new(0),
        mpp_query_ts: AtomicU64::new(0),
        mpp_allocated_task_id: AtomicI64::new(0),
        mpp_allocated_gather_id: AtomicU64::new(0),
        table_stats: HashMap::new(),
        use_chunk_alloc: false,
        tiflash_engine_removed_due_to_strict_sql_mode: false,
        stale_tso_provider: Mutex::new(StaleTSOProvider::default()),
        related_table_ids: HashSet::new(),
        for_share_lock_enabled_by_noop: false,
    }
}

impl StatementContext {
    /// Reset resets a statement context
    ///
    /// Go keeps `CTEStorageMap`, `LockTableIDs`, `TableStats`, `RelatedTableIDs`,
    /// `TblInfo2UnionScan`, the warning handlers, `IndexUsageCollector` and the three
    /// mutex-guarded parts (reset in place); everything else gets its zero value.
    // go: StatementContext.Reset L2 (TryLock always succeeds under &mut self)
    pub fn reset(&mut self) -> bool {
        let warn_handler = self.warn_handler();
        let extra = Arc::clone(&self.extra_warn_handler);
        let lock_table_ids = std::mem::take(&mut self.lock_table_ids);
        let table_stats = std::mem::take(&mut self.table_stats);
        let related_table_ids = std::mem::take(&mut self.related_table_ids);
        let mut mu = std::mem::take(self.mu.get_mut().unwrap_or_else(|e| e.into_inner()));
        let mut stmt_cache =
            std::mem::take(self.stmt_cache.get_mut().unwrap_or_else(|e| e.into_inner()));
        let mut stale = std::mem::take(
            self.stale_tso_provider
                .get_mut()
                .unwrap_or_else(|e| e.into_inner()),
        );
        let warn_sink = Arc::clone(&self.warn_sink);
        *self = new_stmt_ctx_with_time_zone(utc());
        // Go: `*sc = StatementContext{...}` keeps these fields.
        self.warn_sink = warn_sink;
        self.warn_sink.set(Arc::clone(&warn_handler));
        self.extra_warn_handler = Arc::clone(&extra);
        self.lock_table_ids = lock_table_ids;
        self.table_stats = table_stats;
        self.related_table_ids = related_table_ids;
        mu.reset();
        stmt_cache.reset();
        stale.reset();
        *self.mu.get_mut().unwrap_or_else(|e| e.into_inner()) = mu;
        *self.stmt_cache.get_mut().unwrap_or_else(|e| e.into_inner()) = stmt_cache;
        *self
            .stale_tso_provider
            .get_mut()
            .unwrap_or_else(|e| e.into_inner()) = stale;
        // The contexts and trackers point at the statement context (the shared sink).
        let appender: tidb_types::context::WarnHandler = self.warn_sink.clone();
        self.type_ctx = tidb_types::context::new_context(DefaultStmtFlags, utc(), appender.clone());
        self.err_ctx = new_err_ctx(&self.type_ctx, DefaultStmtErrLevels, appender.clone());
        self.plan_cache_tracker = Arc::new(new_plan_cache_tracker(Some(appender.clone())));
        self.range_fallback_handler =
            new_range_fallback_handler(Arc::clone(&self.plan_cache_tracker), appender);
        if !warn_handler.try_reset() {
            self.warn_sink.set(Arc::new(new_static_warn_handler(0)));
        }
        if !extra.try_reset() {
            self.extra_warn_handler = Arc::new(new_static_warn_handler(0));
        }
        true
    }

    /// ResetAlternativeLogicalPlanSignals clears the statement-local signals used by the
    /// alternative logical plan feature.
    // go: StatementContext.ResetAlternativeLogicalPlanSignals L1
    pub fn reset_alternative_logical_plan_signals(&mut self) {
        self.alternative_logical_plan_decorrelated_apply = false;
        self.alternative_logical_plan_same_order_index_join = false;
        self.alternative_logical_plan_prefer_correlate = false;
    }

    /// MarkAlternativeLogicalPlanDecorrelatedApply records that at least one Apply has been
    /// decorrelated into a Join in the current round.
    // go: StatementContext.MarkAlternativeLogicalPlanDecorrelatedApply L1
    pub fn mark_alternative_logical_plan_decorrelated_apply(&mut self) {
        self.alternative_logical_plan_decorrelated_apply = true;
    }

    /// MarkAlternativeLogicalPlanSameOrderIndexJoin records that the current first round has
    /// already produced a same-order index join candidate for a decorrelated Apply.
    // go: StatementContext.MarkAlternativeLogicalPlanSameOrderIndexJoin L1
    pub fn mark_alternative_logical_plan_same_order_index_join(&mut self) {
        self.alternative_logical_plan_same_order_index_join = true;
    }

    /// MarkAlternativeLogicalPlanPreferCorrelate records that the current logical build
    /// round encountered a non-correlated IN subquery that is eligible for the
    /// correlate-to-Apply alternative.
    // go: StatementContext.MarkAlternativeLogicalPlanPreferCorrelate L1
    pub fn mark_alternative_logical_plan_prefer_correlate(&mut self) {
        self.alternative_logical_plan_prefer_correlate = true;
    }

    /// CtxID returns the context id of the statement
    // go: StatementContext.CtxID L1
    pub fn ctx_id(&self) -> u64 {
        self.ctx_id
    }

    /// TimeZone returns the timezone of the type context
    // go: StatementContext.TimeZone L1
    pub fn time_zone(&self) -> Arc<Location> {
        self.type_ctx.location()
    }

    /// SetTimeZone sets the timezone
    // go: StatementContext.SetTimeZone L1
    pub fn set_time_zone(&mut self, tz: Arc<Location>) {
        self.type_ctx = self.type_ctx.with_location(tz);
    }

    /// TypeCtx returns the type context
    // go: StatementContext.TypeCtx L1
    pub fn type_ctx(&self) -> TypeContext {
        self.type_ctx.clone()
    }

    /// ErrCtx returns the error context
    // go: StatementContext.ErrCtx L1
    pub fn err_ctx(&self) -> errctx::Context {
        self.err_ctx.clone()
    }

    /// SetErrLevels sets the error levels for statement The argument otherLevels is used to
    /// set the error levels except truncate
    // go: StatementContext.SetErrLevels L1
    pub fn set_err_levels(&mut self, other_levels: errctx::LevelMap) {
        self.err_ctx = new_err_ctx(&self.type_ctx, other_levels, self.warn_sink.clone());
    }

    /// ErrLevels returns the current `errctx.LevelMap`
    // go: StatementContext.ErrLevels L1
    pub fn err_levels(&self) -> errctx::LevelMap {
        self.err_ctx.level_map()
    }

    /// ErrGroupLevel returns the error level for the given error group
    // go: StatementContext.ErrGroupLevel L1
    pub fn err_group_level(&self, group: errctx::ErrGroup) -> errctx::Level {
        self.err_ctx.level_for_group(group)
    }

    /// TypeFlags returns the type flags
    // go: StatementContext.TypeFlags L1
    pub fn type_flags(&self) -> Flags {
        self.type_ctx.flags()
    }

    /// SetTypeFlags sets the type flags
    // go: StatementContext.SetTypeFlags L1
    pub fn set_type_flags(&mut self, flags: Flags) {
        self.type_ctx = self.type_ctx.with_flags(flags);
        self.err_ctx = new_err_ctx(
            &self.type_ctx,
            self.err_ctx.level_map(),
            self.warn_sink.clone(),
        );
    }

    /// HandleTruncate ignores or returns the error based on the TypeContext inside.
    // go: StatementContext.HandleTruncate L1
    pub fn handle_truncate(&self, err: Result<(), Error>) -> Result<(), Error> {
        self.type_ctx.handle_truncate(err)
    }

    /// HandleError handles the error based on `ErrCtx()`
    // go: StatementContext.HandleError L1
    pub fn handle_error(&self, err: Error) -> Result<(), Error> {
        self.err_ctx().handle_error(err)
    }

    /// HandleErrorWithAlias handles the error based on `ErrCtx()`
    // go: StatementContext.HandleErrorWithAlias L1
    pub fn handle_error_with_alias(
        &self,
        internal_err: Error,
        err: Error,
        warn_err: Error,
    ) -> Result<(), Error> {
        self.err_ctx()
            .handle_error_with_alias(internal_err, err, warn_err)
    }

    /// GetOrStoreStmtCache gets the cached value of the given key if it exists, otherwise
    /// stores the value.
    // go: StatementContext.GetOrStoreStmtCache L1
    pub fn get_or_store_stmt_cache(
        &self,
        key: StmtCacheKey,
        value: StmtCacheValue,
    ) -> StmtCacheValue {
        let mut c = lock(&self.stmt_cache);
        let data = c.data.get_or_insert_with(HashMap::new);
        Arc::clone(data.entry(key).or_insert(value))
    }

    /// GetOrEvaluateStmtCache gets the cached value of the given key if it exists,
    /// otherwise calculate the value.
    // go: StatementContext.GetOrEvaluateStmtCache L1
    pub fn get_or_evaluate_stmt_cache(
        &self,
        key: StmtCacheKey,
        value_evaluator: impl FnOnce() -> Result<StmtCacheValue, Error>,
    ) -> Result<StmtCacheValue, Error> {
        let mut c = lock(&self.stmt_cache);
        let data = c.data.get_or_insert_with(HashMap::new);
        if let std::collections::hash_map::Entry::Vacant(e) = data.entry(key) {
            e.insert(value_evaluator()?);
        }
        Ok(Arc::clone(&data[&key]))
    }

    /// ResetInStmtCache resets the cache of given key.
    // go: StatementContext.ResetInStmtCache L1
    pub fn reset_in_stmt_cache(&self, key: StmtCacheKey) {
        if let Some(data) = lock(&self.stmt_cache).data.as_mut() {
            data.remove(&key);
        }
    }

    /// ResetStmtCache resets all cached values.
    // go: StatementContext.ResetStmtCache L1
    pub fn reset_stmt_cache(&self) {
        lock(&self.stmt_cache).data = Some(HashMap::new());
    }

    /// GetPlan gets the plan field of stmtctx
    // go: StatementContext.GetPlan L1
    pub fn get_plan(&self) -> Option<Arc<dyn Any + Send + Sync>> {
        self.plan.clone()
    }

    /// SetPlan sets the plan field of stmtctx
    // go: StatementContext.SetPlan L1
    pub fn set_plan(&mut self, plan: Option<Arc<dyn Any + Send + Sync>>) {
        self.plan = plan;
    }

    /// GetFlatPlan gets the flatPlan field of stmtctx
    // go: StatementContext.GetFlatPlan L1
    pub fn get_flat_plan(&self) -> Option<Arc<dyn Any + Send + Sync>> {
        self.flat_plan.clone()
    }

    /// SetFlatPlan sets the flatPlan field of stmtctx
    // go: StatementContext.SetFlatPlan L1
    pub fn set_flat_plan(&mut self, flat: Option<Arc<dyn Any + Send + Sync>>) {
        self.flat_plan = flat;
    }

    /// GetBinaryPlan gets the binaryPlan field of stmtctx
    // go: StatementContext.GetBinaryPlan L1
    pub fn get_binary_plan(&self) -> &str {
        &self.binary_plan
    }

    /// SetBinaryPlan sets the binaryPlan field of stmtctx
    // go: StatementContext.SetBinaryPlan L1
    pub fn set_binary_plan(&mut self, binary_plan: String) {
        self.binary_plan = binary_plan;
    }

    /// SetUseChunkAlloc set use chunk alloc status
    // go: StatementContext.SetUseChunkAlloc L1
    pub fn set_use_chunk_alloc(&mut self) {
        self.use_chunk_alloc = true;
    }

    /// ClearUseChunkAlloc clear useChunkAlloc status
    // go: StatementContext.ClearUseChunkAlloc L1
    pub fn clear_use_chunk_alloc(&mut self) {
        self.use_chunk_alloc = false;
    }

    /// GetUseChunkAllocStatus returns useChunkAlloc status
    // go: StatementContext.GetUseChunkAllocStatus L1
    pub fn get_use_chunk_alloc_status(&self) -> bool {
        self.use_chunk_alloc
    }

    /// GetEncodedPlan gets the encoded plan, it is used to avoid repeated encode.
    // go: StatementContext.GetEncodedPlan L1
    pub fn get_encoded_plan(&self) -> &str {
        &self.encoded_plan
    }

    /// SetEncodedPlan sets the encoded plan, it is used to avoid repeated encode.
    // go: StatementContext.SetEncodedPlan L1
    pub fn set_encoded_plan(&mut self, encoded_plan: String) {
        self.encoded_plan = encoded_plan;
    }

    /// GetPlanHint gets the hint string generated from the plan.
    // go: StatementContext.GetPlanHint L1
    pub fn get_plan_hint(&self) -> (&str, bool) {
        (&self.plan_hint, self.plan_hint_set)
    }

    /// GetIndexForce gets the IndexForce boolean generated from the plan.
    // go: StatementContext.GetIndexForce L1
    pub fn get_index_force(&self) -> bool {
        self.index_force
    }

    /// SetPlanHint sets the hint for the plan.
    // go: StatementContext.SetPlanHint L1
    pub fn set_plan_hint(&mut self, hint: String) {
        self.plan_hint_set = true;
        self.plan_hint = hint;
    }

    /// SetIndexForce sets the IndexForce boolean for the plan.
    // go: StatementContext.SetIndexForce L1
    pub fn set_index_force(&mut self) {
        self.index_force = true;
    }

    /// SetHintWarning sets the hint warning and records the reason.
    // go: StatementContext.SetHintWarning L1
    pub fn set_hint_warning(&self, reason: &str) {
        self.append_warning(
            plannererrors::ErrInternal.fast_gen(reason.to_owned(), Vec::<Arg>::new()),
        );
    }

    /// SetHintWarningFromError sets the hint warning and records the reason.
    // go: StatementContext.SetHintWarningFromError L1
    pub fn set_hint_warning_from_error(&self, reason: Error) {
        self.append_warning(reason);
    }

    /// AddAffectedRows adds affected rows.
    // go: StatementContext.AddAffectedRows L1
    pub fn add_affected_rows(&self, rows: u64) {
        if self.in_handle_foreign_key_trigger {
            // For compatibility with MySQL, not add the affected row cause by the foreign
            // key trigger.
            return;
        }
        let mut mu = lock(&self.mu);
        mu.affected_rows = mu.affected_rows.wrapping_add(rows);
    }

    /// SetAffectedRows sets affected rows.
    // go: StatementContext.SetAffectedRows L1
    pub fn set_affected_rows(&self, rows: u64) {
        lock(&self.mu).affected_rows = rows;
    }

    /// AffectedRows gets affected rows.
    // go: StatementContext.AffectedRows L1
    pub fn affected_rows(&self) -> u64 {
        lock(&self.mu).affected_rows
    }

    /// FoundRows gets found rows.
    // go: StatementContext.FoundRows L1
    pub fn found_rows(&self) -> u64 {
        lock(&self.mu).found_rows
    }

    /// AddFoundRows adds found rows.
    // go: StatementContext.AddFoundRows L1
    pub fn add_found_rows(&self, rows: u64) {
        let mut mu = lock(&self.mu);
        mu.found_rows = mu.found_rows.wrapping_add(rows);
    }

    /// RecordRows is used to generate info message
    // go: StatementContext.RecordRows L1
    pub fn record_rows(&self) -> u64 {
        lock(&self.mu).records
    }

    /// AddRecordRows adds record rows.
    // go: StatementContext.AddRecordRows L1
    pub fn add_record_rows(&self, rows: u64) {
        let mut mu = lock(&self.mu);
        mu.records = mu.records.wrapping_add(rows);
    }

    /// DeletedRows is used to generate info message
    // go: StatementContext.DeletedRows L1
    pub fn deleted_rows(&self) -> u64 {
        lock(&self.mu).deleted
    }

    /// AddDeletedRows adds record rows.
    // go: StatementContext.AddDeletedRows L1
    pub fn add_deleted_rows(&self, rows: u64) {
        let mut mu = lock(&self.mu);
        mu.deleted = mu.deleted.wrapping_add(rows);
    }

    /// UpdatedRows is used to generate info message
    // go: StatementContext.UpdatedRows L1
    pub fn updated_rows(&self) -> u64 {
        lock(&self.mu).updated
    }

    /// AddUpdatedRows adds updated rows.
    // go: StatementContext.AddUpdatedRows L1
    pub fn add_updated_rows(&self, rows: u64) {
        let mut mu = lock(&self.mu);
        mu.updated = mu.updated.wrapping_add(rows);
    }

    /// CopiedRows is used to generate info message
    // go: StatementContext.CopiedRows L1
    pub fn copied_rows(&self) -> u64 {
        lock(&self.mu).copied
    }

    /// AddCopiedRows adds copied rows.
    // go: StatementContext.AddCopiedRows L1
    pub fn add_copied_rows(&self, rows: u64) {
        let mut mu = lock(&self.mu);
        mu.copied = mu.copied.wrapping_add(rows);
    }

    /// TouchedRows is used to generate info message
    // go: StatementContext.TouchedRows L1
    pub fn touched_rows(&self) -> u64 {
        lock(&self.mu).touched
    }

    /// AddTouchedRows adds touched rows.
    // go: StatementContext.AddTouchedRows L1
    pub fn add_touched_rows(&self, rows: u64) {
        let mut mu = lock(&self.mu);
        mu.touched = mu.touched.wrapping_add(rows);
    }

    /// GetMessage returns the extra message of the last executed command, if there is no
    /// message, it returns empty string
    // go: StatementContext.GetMessage L1
    pub fn get_message(&self) -> String {
        lock(&self.mu).message.clone()
    }

    /// SetMessage sets the info message generated by some commands
    // go: StatementContext.SetMessage L1
    pub fn set_message(&self, msg: String) {
        lock(&self.mu).message = msg;
    }

    /// The current `WarnHandler` (Go: the exported field `sc.WarnHandler`).
    // glue: reads the field sc.WarnHandler
    pub fn warn_handler(&self) -> WarnHandlerExtRef {
        self.warn_sink.get()
    }

    /// Assigns `sc.WarnHandler`; the type and error contexts see the new handler.
    // glue: assigns the field sc.WarnHandler
    pub fn set_warn_handler(&self, h: WarnHandlerExtRef) {
        self.warn_sink.set(h);
    }

    /// The statement context as a `WarnAppender` (Go passes `sc`).
    // glue: sc as a contextutil.WarnAppender interface value
    pub fn warn_appender(&self) -> Arc<StmtWarnSink> {
        Arc::clone(&self.warn_sink)
    }

    /// GetWarnings gets warnings.
    // go: StatementContext.GetWarnings L1
    pub fn get_warnings(&self) -> Vec<SQLWarn> {
        self.warn_handler().get_warnings()
    }

    /// CopyWarnings copies the warnings to the dst.
    // go: StatementContext.CopyWarnings L1
    pub fn copy_warnings(&self, dst: Vec<SQLWarn>) -> Vec<SQLWarn> {
        self.warn_handler().copy_warnings(dst)
    }

    /// TruncateWarnings truncates warnings begin from start and returns the truncated
    /// warnings.
    // go: StatementContext.TruncateWarnings L1
    pub fn truncate_warnings(&self, start: i64) -> Vec<SQLWarn> {
        self.warn_handler().truncate_warnings(start)
    }

    /// WarningCount gets warning count.
    // go: StatementContext.WarningCount L1
    pub fn warning_count(&self) -> u16 {
        if self.in_show_warning {
            return 0;
        }
        self.warn_handler().warning_count() as u16
    }

    /// NumErrorWarnings gets warning and error count.
    // go: StatementContext.NumErrorWarnings L1
    pub fn num_error_warnings(&self) -> (u16, i64) {
        self.warn_handler().num_error_warnings()
    }

    /// SetWarnings sets warnings.
    // go: StatementContext.SetWarnings L1
    pub fn set_warnings(&self, warns: Vec<SQLWarn>) {
        self.warn_handler().set_warnings(warns);
    }

    /// AppendWarning appends a warning with level 'Warning'.
    // go: StatementContext.AppendWarning L1
    pub fn append_warning(&self, warn: Error) {
        self.warn_handler().append_warning(warn);
    }

    /// AppendWarnings appends some warnings.
    // go: StatementContext.AppendWarnings L1
    pub fn append_warnings(&self, warns: &[SQLWarn]) {
        self.warn_handler().append_warnings(warns);
    }

    /// AppendNote appends a warning with level 'Note'.
    // go: StatementContext.AppendNote L1
    pub fn append_note(&self, warn: Error) {
        self.warn_handler().append_note(warn);
    }

    /// AppendError appends a warning with level 'Error'.
    // go: StatementContext.AppendError L1
    pub fn append_error(&self, warn: Error) {
        self.warn_handler().append_error(warn);
    }

    /// GetExtraWarnings gets extra warnings.
    // go: StatementContext.GetExtraWarnings L1
    pub fn get_extra_warnings(&self) -> Vec<SQLWarn> {
        self.extra_warn_handler.get_warnings()
    }

    /// SetExtraWarnings sets extra warnings.
    // go: StatementContext.SetExtraWarnings L1
    pub fn set_extra_warnings(&self, warns: Vec<SQLWarn>) {
        self.extra_warn_handler.set_warnings(warns);
    }

    /// AppendExtraWarning appends an extra warning with level 'Warning'.
    // go: StatementContext.AppendExtraWarning L1
    pub fn append_extra_warning(&self, warn: Error) {
        self.extra_warn_handler.append_warning(warn);
    }

    /// AppendExtraNote appends an extra warning with level 'Note'.
    // go: StatementContext.AppendExtraNote L1
    pub fn append_extra_note(&self, warn: Error) {
        self.extra_warn_handler.append_note(warn);
    }

    /// AppendExtraError appends an extra warning with level 'Error'.
    // go: StatementContext.AppendExtraError L1
    pub fn append_extra_error(&self, warn: Error) {
        self.extra_warn_handler.append_error(warn);
    }

    // go: StatementContext.resetMuForRetry L1
    fn reset_mu_for_retry(&self) {
        let mut mu = lock(&self.mu);
        mu.affected_rows = 0;
        mu.found_rows = 0;
        mu.records = 0;
        mu.deleted = 0;
        mu.updated = 0;
        mu.copied = 0;
        mu.touched = 0;
        mu.message.clear();
    }

    /// ResetForRetry resets the changed states during execution.
    ///
    /// `SyncExecDetails.Reset` (D14.0) and the DistSQL context cache (part 2) are not here.
    // go: StatementContext.ResetForRetry L2 (no SyncExecDetails / DistSQL context cache yet)
    pub fn reset_for_retry(&mut self) {
        self.reset_mu_for_retry();
        self.reserved_row_id_alloc.reset(0, 0);
        self.table_ids.clear();
        self.index_names.clear();
        self.task_id = allocate_task_id();
        self.warn_handler().truncate_warnings(0);
        self.extra_warn_handler.truncate_warnings(0);
    }

    /// PushDownFlags converts StatementContext to tipb.SelectRequest.Flags.
    // go: StatementContext.PushDownFlags L1
    pub fn push_down_flags(&self) -> u64 {
        let ec = self.err_ctx();
        let mut flags =
            push_down_flags_with_type_flags_and_err_levels(self.type_flags(), ec.level_map());
        if self.in_insert_stmt {
            flags |= tidb_model::FlagInInsertStmt as u64;
        } else if self.in_update_stmt || self.in_delete_stmt {
            flags |= tidb_model::FlagInUpdateOrDeleteStmt as u64;
        } else if self.in_select_stmt {
            flags |= tidb_model::FlagInSelectStmt as u64;
        }
        if self.in_load_data_stmt {
            flags |= tidb_model::FlagInLoadDataStmt as u64;
        }
        if self.in_restricted_sql {
            flags |= tidb_model::FlagInRestrictedSQL as u64;
        }
        flags
    }

    /// InitFromPBFlagAndTz set the flag and timezone of StatementContext from a
    /// `tipb.SelectRequest.Flags` and `*time.Location`.
    // go: StatementContext.InitFromPBFlagAndTz L1
    pub fn init_from_pb_flag_and_tz(&mut self, flags: u64, tz: Arc<Location>) {
        self.in_insert_stmt = (flags & tidb_model::FlagInInsertStmt as u64) > 0;
        self.in_select_stmt = (flags & tidb_model::FlagInSelectStmt as u64) > 0;
        self.in_delete_stmt = (flags & tidb_model::FlagInUpdateOrDeleteStmt as u64) > 0;
        let mut levels = self.err_levels();
        levels[errctx::ErrGroupDividedByZero.0 as usize] = errctx::resolve_err_level(
            false,
            (flags & tidb_model::FlagDividedByZeroAsWarning as u64) > 0,
        );
        self.set_err_levels(levels);
        self.set_time_zone(tz);
        self.set_type_flags(
            DefaultStmtFlags
                .with_ignore_truncate_err((flags & tidb_model::FlagIgnoreTruncate) > 0)
                .with_truncate_as_warning((flags & tidb_model::FlagTruncateAsWarning as u64) > 0)
                .with_ignore_zero_in_date((flags & tidb_model::FlagIgnoreZeroInDate as u64) > 0)
                .with_allow_negative_to_unsigned(!self.in_insert_stmt),
        );
    }

    /// GetLockWaitStartTime returns the statement pessimistic lock wait start time
    /// (Unix nanoseconds; Go returns `time.Unix(0, startTime)`).
    // go: StatementContext.GetLockWaitStartTime L1
    pub fn get_lock_wait_start_time(&self) -> i64 {
        let mut start_time = self.lock_wait_start_time.load(Ordering::SeqCst);
        if start_time == 0 {
            start_time = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos() as i64);
            self.lock_wait_start_time
                .store(start_time, Ordering::SeqCst);
        }
        start_time
    }

    /// UseDynamicPartitionPrune indicates whether dynamic partition is used during the
    /// query
    // go: StatementContext.UseDynamicPartitionPrune L1
    pub fn use_dynamic_partition_prune(&self) -> bool {
        self.use_dynamic_prune_mode
    }

    /// SetStaleTSOProviderIfNotExist sets the stale TSO provider if not exist.
    // go: StatementContext.SetStaleTSOProviderIfNotExist L1
    pub fn set_stale_tso_provider_if_not_exist(&self, eval: StaleTSOEval) {
        let mut p = lock(&self.stale_tso_provider);
        if p.eval.is_some() {
            return;
        }
        p.value = None;
        p.eval = Some(eval);
    }

    /// GetStaleTSO returns the TSO for stale-read usage which calculate from PD's last
    /// response.
    // go: StatementContext.GetStaleTSO L1
    pub fn get_stale_tso(&self) -> Result<u64, Error> {
        let mut p = lock(&self.stale_tso_provider);
        if let Some(v) = p.value {
            return Ok(v);
        }
        let Some(eval) = p.eval.as_mut() else {
            return Ok(0);
        };
        let tso = eval()?;
        p.value = Some(tso);
        Ok(tso)
    }

    /// AddSetVarHintRestore records the variables which are affected by SET_VAR hint. And
    /// restore them to the old value later.
    // go: StatementContext.AddSetVarHintRestore L1
    pub fn add_set_var_hint_restore(&mut self, name: String, val: String) {
        self.set_var_hint_restore
            .get_or_insert_with(HashMap::new)
            .entry(name)
            .or_insert(val);
    }

    /// GetOrInitRangerCtxFromCache returns the `RangerContext` inside cache. If it didn't
    /// exist, return a new one created by the `create` function. It uses the `any` to avoid
    /// cycle dependency.
    // go: StatementContext.GetOrInitRangerCtxFromCache L1
    pub fn get_or_init_ranger_ctx_from_cache(
        &self,
        create: impl FnOnce() -> Arc<dyn Any + Send + Sync>,
    ) -> Arc<dyn Any + Send + Sync> {
        Arc::clone(self.ranger_ctx_cache.get_or_init(create))
    }

    /// GetOrInitBuildPBCtxFromCache returns the `BuildPBContext` inside cache. If it didn't
    /// exist, return a new one created by the `create` function. It uses the `any` to avoid
    /// cycle dependency.
    // go: StatementContext.GetOrInitBuildPBCtxFromCache L1
    pub fn get_or_init_build_pb_ctx_from_cache(
        &self,
        create: impl FnOnce() -> Arc<dyn Any + Send + Sync>,
    ) -> Arc<dyn Any + Send + Sync> {
        Arc::clone(self.build_pb_ctx_cache.get_or_init(create))
    }

    /// The normalized plan of `SetPlanDigest` (the digest itself waits for `parser.Digest`).
    // glue: planNormalized of GetPlanDigest without the digest
    pub fn plan_normalized(&self) -> &str {
        &self.plan_normalized
    }
}

/// TypeCtxOrDefault returns the reference to the `TypeCtx` inside the statement context. If
/// the statement context is nil, it'll return a newly created default type context.
/// **don't** use this function if you can make sure the `StatementContext` is not nil.
/// Otherwise, this function will hide the issue.
// go: StatementContext.TypeCtxOrDefault L1
pub fn type_ctx_or_default(sc: Option<&StatementContext>) -> TypeContext {
    match sc {
        Some(sc) => sc.type_ctx.clone(),
        None => DefaultStmtNoWarningContext.clone(),
    }
}

/// PushDownFlagsWithTypeFlagsAndErrLevels applies all type flags and error levels to
/// tipb.SelectRequest.Flags.
// go: PushDownFlagsWithTypeFlagsAndErrLevels L1
pub fn push_down_flags_with_type_flags_and_err_levels(
    tc_flags: Flags,
    err_levels: errctx::LevelMap,
) -> u64 {
    let mut flags: u64 = 0;
    if tc_flags.ignore_truncate_err() {
        flags |= tidb_model::FlagIgnoreTruncate;
    } else if tc_flags.truncate_as_warning() {
        flags |= tidb_model::FlagTruncateAsWarning as u64;
        // TODO: remove this flag from TiKV.
        flags |= tidb_model::FlagOverflowAsWarning as u64;
    }
    if tc_flags.ignore_zero_in_date() {
        flags |= tidb_model::FlagIgnoreZeroInDate as u64;
    }
    if err_levels[errctx::ErrGroupDividedByZero.0 as usize] != errctx::LevelError {
        flags |= tidb_model::FlagDividedByZeroAsWarning as u64;
    }
    flags
}

// go: newErrCtx L1
fn new_err_ctx(
    tc: &TypeContext,
    mut other_levels: errctx::LevelMap,
    handler: tidb_types::context::WarnHandler,
) -> errctx::Context {
    let flags = tc.flags();
    let l = if flags.ignore_truncate_err() {
        errctx::LevelIgnore
    } else if flags.truncate_as_warning() {
        errctx::LevelWarn
    } else {
        errctx::LevelError
    };
    other_levels[errctx::ErrGroupTruncate.0 as usize] = l;
    errctx::new_context_with_levels(other_levels, handler)
}
