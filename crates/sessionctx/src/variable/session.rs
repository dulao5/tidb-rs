//! go-file: pkg/sessionctx/variable/session.go
//!
//! The part of `SessionVars` (pkg/sessionctx/variable/session.go, block D05.2) and of
//! `stmtctx.StatementContext` (D05.2) that the D05.1 validation code reads or writes.
//!
//! stand-in: D05.2 (not counted by converge: every item is `glue:`). Block D05.2 replaces
//! this file with the faithful `SessionVars` (N2 /
//! N8 §3.2); the fields here keep Go's names and meaning so the validation hooks do not
//! change when it does. Only what the validation hooks touch is here.

#![allow(non_upper_case_globals)]

use std::collections::HashMap;
use std::sync::Arc;

use tidb_base::Error;
use tidb_base::gostd::time::Location;

use tidb_base::gofmt::Arg;

use super::error::{ErrNotSupportedYet, ErrUnknownSystemVar};
use super::names::{CharacterSetConnection, CollationConnection, ErrorCount, WarningCount};
use super::variable::{ScopeGlobal, ScopeSession, get_sys_var};

pub use crate::stmtctx::StatementContext;
pub use tidb_base::contextutil::{SQLWarn, WarnLevelWarning};

/// GlobalVarAccessor is the interface for accessing global scope system and status variables.
// go-part: GlobalVarAccessor (variable.go interface) L2 (read-path methods only; synchronous, like Go)
pub trait GlobalVarAccessor: Send {
    /// GetGlobalSysVar gets the global system variable value for name.
    // go-part: GlobalVarAccessor.GetGlobalSysVar (interface method)
    fn get_global_sys_var(&self, name: &str) -> Result<String, Error>;
    /// SetGlobalSysVarOnly sets the global system variable without calling the validation
    /// function or updating aliases.
    // go-part: GlobalVarAccessor.SetGlobalSysVarOnly (interface method)
    fn set_global_sys_var_only(
        &mut self,
        name: &str,
        value: &str,
        update_local: bool,
    ) -> Result<(), Error>;
    /// GetTiDBTableValue gets a value from mysql.tidb for the key 'name'
    // go-part: GlobalVarAccessor.GetTiDBTableValue (interface method)
    fn get_tidb_table_value(&self, name: &str) -> Result<String, Error>;
    /// SetTiDBTableValue sets a value+comment for the mysql.tidb key 'name'
    // go-part: GlobalVarAccessor.SetTiDBTableValue (interface method)
    fn set_tidb_table_value(&mut self, name: &str, value: &str, comment: &str)
    -> Result<(), Error>;
}

/// The fields of `SessionVars` the D05.1 validation hooks use.
// glue: stand-in: D05.2, SessionVars
pub struct SessionVars {
    /// StmtCtx holds variables for current executing statement (Go `*StatementContext`;
    /// shared, so other holders such as `ProcessInfo` keep it alive — N2). Read with
    /// [`SessionVars::stmt_ctx`], replace with [`SessionVars::set_stmt_ctx`].
    stmt_ctx: Arc<StatementContext>,
    /// The `useChunkAlloc` cell of the current statement context, which the chunk
    /// allocator's reuse hook sets: Go's hook reads `s.StmtCtx` when it fires, which may be
    /// a context installed after `SetAlloc`. Holding the cell (not the context) keeps
    /// `stmt_ctx_mut` working.
    stmt_ctx_slot: Arc<parking_lot::Mutex<Arc<std::sync::atomic::AtomicBool>>>,
    /// chunkPool Several chunks and columns are cached
    chunk_pool: Option<Box<dyn tidb_chunk::alloc::Allocator>>,
    /// GlobalVarsAccessor is used to set and get global variables.
    pub global_vars_accessor: Box<dyn GlobalVarAccessor>,
    /// systems variables, don't modify it directly, use GetSystemVar/SetSystemVar method.
    pub(crate) systems: HashMap<String, String>,
    /// The scalar fields of Go's `SessionVars` (generated, D05.2 T2), reached through
    /// `Deref` as Go code reads them on the struct itself.
    pub fields: super::session_fields::SessionVarsFields,
    /// TimeZone is the time zone of the session (`nil`: the system location).
    pub time_zone: Option<Arc<Location>>,
    /// TLSConnectionState is the TLS state of the connection (only its presence is read here).
    pub tls_connection_state: Option<()>,
    /// The non-scalar fields of Go's `SessionVars` that the session hooks use (D05.2 T6).
    pub state: SessionState,
}

/// The non-scalar fields of Go's `SessionVars` the session hooks read or write (D05.2 T6).
/// Go's map-typed sets (`map[kv.StoreType]struct{}`, `map[string]struct{}`) are ordered sets
/// here (M-MAPORDER: only membership is observable).
// glue: the non-scalar part of SessionVars (field names as in Go)
#[derive(Clone, Debug)]
pub struct SessionState {
    /// status stands for the session status. e.g. in transaction or not, auto commit is on or off, and so on.
    pub status: u32,
    /// TxnCtx Should be reset on transaction finished.
    pub txn_ctx: super::session_state::TransactionContext,
    /// KVVars is the variables for KV storage.
    pub kv_vars: super::session_state::KVVars,
    /// txnIsolationLevelOneShot is used to implements "set transaction isolation level ..."
    pub txn_isolation_level_one_shot: super::session_state::TxnIsolationLevelOneShot,
    /// TxnReadTS is used for staleness transaction, it provides next staleness transaction startTS.
    pub txn_read_ts: Option<super::session_state::TxnReadTS>,
    /// IsolationReadEngines is used to isolation read, tidb only read from the stores whose engine type is in the engines.
    pub isolation_read_engines: std::collections::BTreeSet<tidb_base::kv::StoreType>,
    /// AllowFallbackToTiKV indicates the engine types whose unavailability triggers fallback to TiKV.
    pub allow_fallback_to_tikv: std::collections::BTreeSet<tidb_base::kv::StoreType>,
    /// PartitionPruneMode indicates how and when to prune partitions.
    pub partition_prune_mode: String,
    /// TxnScope indicates the scope of the transactions.
    pub txn_scope: tidb_kv::txn_scope_var::TxnScopeVar,
    /// Rng stores the rand_seed1 and rand_seed2 for Rand() function
    pub rng: super::session_state::MysqlRng,
    /// StatsLoadSyncWait indicates how long to wait for stats load before timeout.
    pub stats_load_sync_wait: i64,
    /// The bytes limit of MemTracker (`MemTracker.SetBytesLimit`).
    pub mem_tracker_bytes_limit: i64,
    /// OptimizerFixControl control some details of the optimizer behavior through the tidb_opt_fix_control variable.
    pub optimizer_fix_control: HashMap<u64, String>,
    /// runtimeFilterTypes the types of runtime filter.
    pub runtime_filter_types: Vec<super::session_state::RuntimeFilterType>,
    /// AnalyzeSkipColumnTypes indicates the column types whose statistics would not be collected when executing the ANALYZE command.
    pub analyze_skip_column_types: std::collections::BTreeSet<String>,
    /// SlowLogRules holds the slow log rules of the session.
    pub slow_log_rules: super::session_state::SessionSlowLogRules,
    /// RowEncoder is reused in session for encode row data (`RowEncoder.Enable`).
    pub row_encoder_enable: bool,
    /// LastQueryInfo keeps track the info of last query.
    pub last_query_info: super::session_state::QueryInfo,
    /// LastDDLInfo keeps track the info of last DDL.
    pub last_ddl_info: super::session_state::LastDDLInfo,
    /// SnapshotInfoschema is used with SnapshotTS (only whether it is set is kept).
    pub snapshot_infoschema: bool,
    /// UserVars is used to provide user variable operations.
    pub user_vars: Arc<super::uservars::UserVars>,
    /// PlanCacheParams is used to provide parameters for plan cache.
    pub plan_cache_params: super::plan_cache_params::PlanCacheParamList,
    /// PreparedStmts stores prepared statement (Go `map[uint32]any`; the planner's
    /// `PlanCacheStmt`).
    pub prepared_stmts: HashMap<u32, Arc<dyn std::any::Any + Send + Sync>>,
    /// PreparedStmtNameToID maps a prepared statement name to its ID.
    pub prepared_stmt_name_to_id: HashMap<String, u32>,
    /// PlanID is the unique id of logical and physical plan.
    pub plan_id: super::session_state::AtomicI32Field,
    /// PlanColumnID is the unique id for column when building plan.
    pub plan_column_id: super::session_state::AtomicI64Field,
    /// MapScalarSubQ maps the scalar sub queries from its ID to its struct.
    pub map_scalar_sub_q: Vec<Arc<dyn std::any::Any + Send + Sync>>,
    /// PlanReplayerFinishedTaskKey used to record the finished plan replayer task key in order not to record the
    /// duplicate task in plan replayer continues capture
    pub plan_replayer_finished_task_key:
        Option<std::collections::HashSet<super::session_state::PlanReplayerTaskKey>>,
}

impl Default for SessionState {
    // go-part: NewSessionVars L2 (the non-scalar fields; the store engines come from config.IsolationRead.Engines)
    fn default() -> Self {
        use tidb_base::kv::{TiDB, TiFlash, TiKV};
        let mut isolation_read_engines = std::collections::BTreeSet::new();
        for engine in tidb_base::config::get_global_config()
            .isolation_read
            .engines
            .iter()
            .flatten()
        {
            if engine == TiFlash.name() {
                isolation_read_engines.insert(TiFlash);
            } else if engine == TiKV.name() {
                isolation_read_engines.insert(TiKV);
            } else if engine == TiDB.name() {
                isolation_read_engines.insert(TiDB);
            }
        }
        let txn_scope =
            if !super::tidb_vars::EnableLocalTxn.load(std::sync::atomic::Ordering::SeqCst) {
                tidb_kv::txn_scope_var::new_global_txn_scope_var()
            } else {
                tidb_kv::txn_scope_var::new_default_txn_scope_var()
            };
        SessionState {
            status: u32::from(tidb_base::mysql::ServerStatusAutocommit),
            txn_ctx: Default::default(),
            kv_vars: Default::default(),
            txn_isolation_level_one_shot: Default::default(),
            txn_read_ts: None,
            isolation_read_engines,
            allow_fallback_to_tikv: Default::default(),
            partition_prune_mode: super::tidb_defs::DefTiDBPartitionPruneMode.to_owned(),
            txn_scope,
            rng: super::session_state::MysqlRng::new_with_time(),
            stats_load_sync_wait: super::tidb_vars::StatsLoadSyncWait
                .load(std::sync::atomic::Ordering::SeqCst),
            mem_tracker_bytes_limit: super::tidb_vars::DefTiDBMemQuotaQuery,
            optimizer_fix_control: HashMap::new(),
            runtime_filter_types: Vec::new(),
            analyze_skip_column_types: Default::default(),
            slow_log_rules: Default::default(),
            row_encoder_enable: false,
            last_query_info: Default::default(),
            last_ddl_info: Default::default(),
            snapshot_infoschema: false,
            user_vars: super::uservars::new_user_vars(),
            plan_cache_params: super::plan_cache_params::new_plan_cache_param_list(),
            prepared_stmts: HashMap::new(),
            prepared_stmt_name_to_id: HashMap::new(),
            plan_id: Default::default(),
            plan_column_id: Default::default(),
            map_scalar_sub_q: Vec::new(),
            plan_replayer_finished_task_key: None,
        }
    }
}

impl std::ops::Deref for SessionVars {
    type Target = super::session_fields::SessionVarsFields;
    // glue: Go reads the scalar fields on SessionVars itself
    fn deref(&self) -> &Self::Target {
        &self.fields
    }
}

impl std::ops::DerefMut for SessionVars {
    // glue: Go writes the scalar fields on SessionVars itself
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.fields
    }
}

impl SessionVars {
    /// StmtCtx holds variables for current executing statement.
    // glue: accessor of the StmtCtx field (Go reads s.StmtCtx)
    pub fn stmt_ctx(&self) -> &StatementContext {
        &self.stmt_ctx
    }

    /// The shared handle of the current statement context (Go copies the pointer).
    // glue: accessor of the StmtCtx field
    pub fn stmt_ctx_arc(&self) -> Arc<StatementContext> {
        Arc::clone(&self.stmt_ctx)
    }

    /// Installs the statement context (Go `vars.StmtCtx = sc`).
    // glue: assignment of the StmtCtx field
    pub fn set_stmt_ctx(&mut self, sc: Arc<StatementContext>) {
        *self.stmt_ctx_slot.lock() = sc.use_chunk_alloc_cell();
        self.stmt_ctx = sc;
    }

    /// Mutable access to the statement context's plain fields, for the session thread while
    /// nothing else holds the context (Go writes the fields through the pointer).
    ///
    /// Panics if the context is shared: fields written while the statement runs are atomic
    /// (`set_last_insert_id`, `set_use_chunk_alloc`, the warnings) and go through
    /// [`SessionVars::stmt_ctx`].
    // glue: field writes through s.StmtCtx (N2: the session owns the context until it shares it)
    pub fn stmt_ctx_mut(&mut self) -> &mut StatementContext {
        Arc::get_mut(&mut self.stmt_ctx)
            .expect("statement context is shared; write atomic fields through stmt_ctx()")
    }

    // glue: field access for session_methods.rs (chunkPool, cachedStmtCtx)
    pub(super) fn chunk_pool_mut(&mut self) -> Option<&mut dyn tidb_chunk::alloc::Allocator> {
        match &mut self.chunk_pool {
            Some(p) => Some(p.as_mut()),
            None => None,
        }
    }

    // glue: field access for session_methods.rs
    pub(super) fn set_chunk_pool(&mut self, p: Option<Box<dyn tidb_chunk::alloc::Allocator>>) {
        self.chunk_pool = p;
    }

    // glue: field access for session_methods.rs
    pub(super) fn has_chunk_pool(&self) -> bool {
        self.chunk_pool.is_some()
    }

    // glue: field access for session_methods.rs
    pub(super) fn stmt_ctx_slot(
        &self,
    ) -> Arc<parking_lot::Mutex<Arc<std::sync::atomic::AtomicBool>>> {
        Arc::clone(&self.stmt_ctx_slot)
    }
}

impl SessionVars {
    /// A session with Go's `NewSessionVars` defaults for the fields above.
    // glue: stand-in: D05.2, NewSessionVars
    pub fn new(global_vars_accessor: Box<dyn GlobalVarAccessor>) -> SessionVars {
        let stmt_ctx = Arc::new(crate::stmtctx::new_stmt_ctx());
        SessionVars {
            stmt_ctx_slot: Arc::new(parking_lot::Mutex::new(stmt_ctx.use_chunk_alloc_cell())),
            stmt_ctx,
            chunk_pool: None,
            global_vars_accessor,
            systems: HashMap::from([
                // charset.GetDefaultCharsetAndCollate()
                (CharacterSetConnection.to_owned(), "utf8mb4".to_owned()),
                (CollationConnection.to_owned(), "utf8mb4_bin".to_owned()),
            ]),
            fields: super::session_fields::SessionVarsFields::default(),
            time_zone: None,
            tls_connection_state: None,
            state: SessionState::default(),
        }
    }

    /// GetSessionOrGlobalSystemVar gets a system variable.
    /// If it is a session only variable, use the default value defined in code.
    /// Returns error if there is no such variable.
    // go: SessionVars.GetSessionOrGlobalSystemVar L2 (no context argument)
    pub fn get_session_or_global_system_var(&mut self, name: &str) -> Result<String, Error> {
        let Some(sv) = get_sys_var(name) else {
            return Err(
                ErrUnknownSystemVar.gen_with_stack_by_args(vec![Arg::from(name.to_owned())])
            );
        };
        if sv.has_none_scope() {
            return Ok(sv.value.clone());
        }
        if sv.has_session_scope() {
            // Populate the value to s.systems if it is not there already.
            // in future should be already loaded on session init
            if sv.get_session.is_some() {
                // shortcut to the getter, we won't use the value
                return sv.get_session_from_hook(self);
            }
            if !self.systems.contains_key(sv.name) {
                if sv.has_global_scope() {
                    if let Ok(val) = self.global_vars_accessor.get_global_sys_var(sv.name) {
                        self.systems.insert(sv.name.to_owned(), val);
                    }
                } else {
                    self.systems.insert(sv.name.to_owned(), sv.value.clone()); // no global scope, use default
                }
            }
            return sv.get_session_from_hook(self);
        }
        sv.get_global_from_hook(self)
    }

    /// GetSessionStatesSystemVar gets the session variable value for session states.
    /// It's only used for encoding session states when migrating a session.
    /// The returned boolean indicates whether to keep this value in the session states.
    // go: SessionVars.GetSessionStatesSystemVar L1
    pub fn get_session_states_system_var(&mut self, name: &str) -> Result<(String, bool), Error> {
        let Some(sv) = get_sys_var(name) else {
            return Err(
                ErrUnknownSystemVar.gen_with_stack_by_args(vec![Arg::from(name.to_owned())])
            );
        };
        // Call GetStateValue first if it exists. Otherwise, call GetSession.
        if let Some(get_state_value) = sv.get_state_value {
            return get_state_value(self);
        }
        if sv.get_session.is_some() {
            let val = sv.get_session_from_hook(self)?;
            return Ok((val, true));
        }
        // Only get the cached value. No need to check the global or default value.
        if let Some(val) = self.systems.get(sv.name) {
            return Ok((val.clone(), true));
        }
        Ok((String::new(), false))
    }

    /// GetGlobalSystemVar gets a global system variable.
    // go: SessionVars.GetGlobalSystemVar L2 (no context argument)
    pub fn get_global_system_var(&mut self, name: &str) -> Result<String, Error> {
        let Some(sv) = get_sys_var(name) else {
            return Err(
                ErrUnknownSystemVar.gen_with_stack_by_args(vec![Arg::from(name.to_owned())])
            );
        };
        sv.get_global_from_hook(self)
    }

    /// SetSystemVar sets the value of a system variable for session scope.
    /// Values are automatically normalized (i.e. oN / on / 1 => ON)
    /// and the validation function is run. To set with less validation, see
    /// SetSystemVarWithRelaxedValidation.
    // go: SessionVars.SetSystemVar L1
    pub fn set_system_var(&mut self, name: &str, val: &str) -> Result<(), Error> {
        let Some(sv) = get_sys_var(name) else {
            return Err(
                ErrUnknownSystemVar.gen_with_stack_by_args(vec![Arg::from(name.to_owned())])
            );
        };
        let (val, err) = sv.validate(self, val, ScopeSession);
        err?;
        sv.set_session_from_hook(self, &val)
    }

    /// SetSystemVarWithOldValAsRet is wrapper of SetSystemVar. Return the old value for later use.
    // go: SessionVars.SetSystemVarWithOldValAsRet L1
    pub fn set_system_var_with_old_val_as_ret(
        &mut self,
        name: &str,
        val: &str,
    ) -> (String, Result<(), Error>) {
        let Some(sv) = get_sys_var(name) else {
            return (
                String::new(),
                Err(ErrUnknownSystemVar.gen_with_stack_by_args(vec![Arg::from(name.to_owned())])),
            );
        };
        let (val, err) = sv.validate(self, val, ScopeSession);
        if let Err(e) = err {
            return (String::new(), Err(e));
        }
        // The map s.systems[sv.Name] is lazy initialized. If we directly read it, we might read empty result.
        // Since this code path is not a hot path, we directly call GetSessionOrGlobalSystemVar to get the value safely.
        let old_v = match self.get_session_or_global_system_var(sv.name) {
            Ok(v) => v,
            Err(e) => return (String::new(), Err(e)),
        };
        let err = sv.set_session_from_hook(self, &val);
        (old_v, err)
    }

    /// SetSystemVarWithoutValidation sets the value of a system variable for session scope.
    /// Deprecated: Values are NOT normalized or Validated.
    // go: SessionVars.SetSystemVarWithoutValidation L1
    pub fn set_system_var_without_validation(
        &mut self,
        name: &str,
        val: &str,
    ) -> Result<(), Error> {
        let Some(sv) = get_sys_var(name) else {
            return Err(
                ErrUnknownSystemVar.gen_with_stack_by_args(vec![Arg::from(name.to_owned())])
            );
        };
        sv.set_session_from_hook(self, val)
    }

    /// SetSystemVarWithRelaxedValidation sets the value of a system variable for session scope.
    /// Validation functions are called, but scope validation is skipped.
    /// Errors are not expected to be returned because this could cause upgrade issues.
    // go: SessionVars.SetSystemVarWithRelaxedValidation L1
    pub fn set_system_var_with_relaxed_validation(
        &mut self,
        name: &str,
        val: &str,
    ) -> Result<(), Error> {
        let Some(sv) = get_sys_var(name) else {
            return Err(
                ErrUnknownSystemVar.gen_with_stack_by_args(vec![Arg::from(name.to_owned())])
            );
        };
        let val = sv.validate_with_relaxed_validation(self, val, ScopeSession);
        sv.set_session_from_hook(self, &val)
    }

    /// `SET GLOBAL` in the read-only phase (owner issue #58, option (a); session.go
    /// `session.SetGlobalSysVar`): the value is validated as in Go; INSTANCE-scope variables
    /// run their SetGlobal hook (Go returns before persisting them too); any other variable
    /// would be persisted to mysql.global_variables by Go, which tidb-rs does not write:
    /// it returns "not supported".
    // glue: session.SetGlobalSysVar (block D05.3) restricted to the read-only phase
    pub fn set_global_sys_var(&mut self, name: &str, value: &str) -> Result<(), Error> {
        let Some(sv) = get_sys_var(name) else {
            return Err(
                ErrUnknownSystemVar.gen_with_stack_by_args(vec![Arg::from(name.to_owned())])
            );
        };
        let (value, err) = sv.validate(self, value, ScopeGlobal);
        err?;
        if sv.has_instance_scope() {
            return sv.set_global_from_hook(self, &value, false);
        }
        Err(ErrNotSupportedYet.gen_with_stack_by_args(vec![Arg::from(
            "SET GLOBAL on tidb-rs; run it on Go TiDB".to_owned(),
        )]))
    }

    /// GetSystemVar gets the string value of a system variable.
    // go: SessionVars.GetSystemVar L1
    pub fn get_system_var(&self, name: &str) -> (String, bool) {
        if name == WarningCount {
            return (self.sys_warning_count.to_string(), true);
        } else if name == ErrorCount {
            return (self.sys_error_count.to_string(), true);
        }
        match self.systems.get(name) {
            Some(v) => (v.clone(), true),
            None => (String::new(), false),
        }
    }

    /// The session values (`systems`), for inspection by tests and SHOW (Go reads the map
    /// directly inside the package).
    // glue: read access to SessionVars.systems
    pub fn systems(&self) -> &HashMap<String, String> {
        &self.systems
    }

    /// Sets a session value without validation or hooks (test setup; Go's
    /// `SetSystemVarWithoutValidation` also runs the SetSession hook, block D05.2).
    // glue: stand-in: D05.2 (systems map only)
    pub fn set_system_var_for_test(&mut self, name: &str, value: &str) {
        // SetSessionFromHook writes systems[name] and systems[alias] for every alias.
        self.systems.insert(name.to_owned(), value.to_owned());
        if let Some(sv) = super::variable::get_sys_var(name) {
            for alias in sv.aliases {
                self.systems.insert((*alias).to_owned(), value.to_owned());
            }
        }
    }

    /// Sets allowMPPExecution (Go: the SetSession hook of tidb_allow_mpp).
    // glue: stand-in: D05.2 —
    pub fn set_allow_mpp_execution(&mut self, on: bool) {
        self.allow_mpp_execution = on;
    }

    /// SetStatusFlag sets the session server status variable.
    /// If on is true sets the flag in session status,
    /// otherwise removes the flag.
    // The session owns its status, so no CAS loop is needed.
    // go: SessionVars.SetStatusFlag L1
    pub fn set_status_flag(&mut self, flag: u16, on: bool) {
        if on {
            self.state.status |= u32::from(flag);
        } else {
            self.state.status &= !u32::from(flag);
        }
    }

    /// HasStatusFlag gets the session server status variable, returns true if it is on.
    // go: SessionVars.HasStatusFlag L1
    pub fn has_status_flag(&self, flag: u16) -> bool {
        self.state.status & u32::from(flag) > 0
    }

    /// Status returns the server status.
    // go: SessionVars.Status L1
    pub fn status(&self) -> u16 {
        self.state.status as u16
    }

    /// SetInTxn sets whether the session is in transaction.
    /// It also updates the IsExplicit flag in TxnCtx if val is true.
    // go: SessionVars.SetInTxn L1
    pub fn set_in_txn(&mut self, val: bool) {
        self.set_status_flag(tidb_base::mysql::ServerStatusInTrans, val);
        if val {
            self.state.txn_ctx.is_explicit = val;
        }
    }

    /// InTxn returns if the session is in transaction.
    // go: SessionVars.InTxn L1
    pub fn in_txn(&self) -> bool {
        self.has_status_flag(tidb_base::mysql::ServerStatusInTrans)
    }

    /// IsAutocommit returns if the session is set to autocommit.
    // go: SessionVars.IsAutocommit L1
    pub fn is_autocommit(&self) -> bool {
        self.has_status_flag(tidb_base::mysql::ServerStatusAutocommit)
    }

    /// SetReplicaRead set SessionVars.replicaRead.
    // go: SessionVars.SetReplicaRead L1
    pub fn set_replica_read(&mut self, val: tidb_kv::option::ReplicaReadType) {
        self.replica_read = val.0;
    }

    /// ChooseMppVersion indicates the mpp-version used to build mpp plan, if mpp-version is unspecified, use the latest version.
    // go: SessionVars.ChooseMppVersion L1
    pub fn choose_mpp_version(&self) -> i64 {
        if self.mpp_version == tidb_kv::mpp::MppVersionUnspecified.0 {
            return tidb_kv::mpp::get_newest_mpp_version().0;
        }
        self.mpp_version
    }

    /// Location gets the time zone of the session.
    // go: SessionVars.Location L2 (timeutil.SystemLocation: the process local zone, gostd time::local)
    pub fn location(&self) -> Arc<Location> {
        match &self.time_zone {
            Some(l) => l.clone(),
            None => tidb_base::gostd::time::local(),
        }
    }
}

// SessionVars: Send + !Sync (N2 §5 item 1, D05.2 T9): a session moves between runtime
// threads but is used by one task at a time.
// glue: compile-time assertions
const _: () = {
    // glue: compile-time assertion
    const fn assert_send<T: Send>() {}
    assert_send::<SessionVars>();
};

/// `SessionVars` is `!Sync`: this impl would be ambiguous with the blanket one if it were
/// `Sync` (the usual compile-time negative assertion).
// glue: compile-time assertion that SessionVars is not Sync
#[allow(dead_code)]
trait AmbiguousIfSync<A> {
    // glue: assertion helper
    fn some_item() {}
}
impl<T: ?Sized> AmbiguousIfSync<()> for T {}
#[allow(dead_code)]
struct Invalid;
impl<T: ?Sized + Sync> AmbiguousIfSync<Invalid> for T {}
const _: fn() = || {
    let _ = <SessionVars as AmbiguousIfSync<_>>::some_item;
};
