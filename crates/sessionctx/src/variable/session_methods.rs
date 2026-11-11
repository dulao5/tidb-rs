//! go-file: pkg/sessionctx/variable/session.go
//!
//! The methods of `SessionVars` that the planner, the executor and the protocol layer call
//! (D05.2 PR D), and the small session types they use. The methods that read the statement
//! context's hints or chunk allocator, the prepared-statement caches built on
//! `kvcache.SimpleLRUCache`, session states and the slow log follow in PR E (spec §3.8).

#![allow(non_upper_case_globals)]

use std::any::Any;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering::SeqCst;

use tidb_base::Error;
use tidb_base::dbterror::plannererrors::ErrStmtNotFound;
use tidb_base::gofmt::Arg;
use tidb_kv::option::{ReplicaReadClosestAdaptive, ReplicaReadType};
use tidb_kv::txn_scope_var::{GlobalTxnScope, LocalTxnScope};
use tidb_model::{TableInfo, TempTableNone};
use tidb_types::datum::{Datum, new_collation_string_datum};

use super::error::ErrMaxPreparedStmtCountReached;
use super::hooks::{AutoStr, ForcePreAggStr, ForceStreamingStr};
use super::names::{CharacterSetConnection, CollationConnection, TxnIsolation};
use super::session::SessionVars;
use super::session_state::{In, MinMax};
use super::session_state::{
    OneShotState, PlanReplayerTaskKey, RFGlobal, RFLocal, RFOff, RuntimeFilterMode,
    RuntimeFilterType, TxnReadTS,
};
use super::tidb_defs::DefTiDBDefaultStrMatchSelectivity;
use super::tidb_vars::{EnableLocalTxn, MaxPreparedStmtCountValue};

/// PreparedStmtCount is exported for test.
pub static PreparedStmtCount: AtomicI64 = AtomicI64::new(0);

/// enableAdaptiveReplicaRead (process-wide; 1 by default).
static enableAdaptiveReplicaRead: AtomicU32 = AtomicU32::new(1);

/// planReplayerSessionFinishedTaskKeyLen is used to control the max size for the finished plan replayer task key in session
/// in order to control the used memory
const planReplayerSessionFinishedTaskKeyLen: usize = 128;

/// `ast.ReadCommitted`.
const ReadCommitted: &str = "READ-COMMITTED";
/// `ast.Optimistic`.
const Optimistic: &str = "OPTIMISTIC";

/// `kv.ExchangeCompressionModeUnspecified` (pkg/kv/mpp.go, DEF.MPP).
pub const ExchangeCompressionModeUnspecified: i64 = 3;
/// `kv.RecommendedExchangeCompressionMode` (= `ExchangeCompressionModeFast`).
pub const RecommendedExchangeCompressionMode: i64 = 1;

/// SetEnableAdaptiveReplicaRead set `enableAdaptiveReplicaRead` with given value.
/// return true if the value is changed.
// go: SetEnableAdaptiveReplicaRead L1
pub fn set_enable_adaptive_replica_read(enabled: bool) -> bool {
    let value = u32::from(enabled);
    enableAdaptiveReplicaRead.swap(value, SeqCst) != value
}

/// IsAdaptiveReplicaReadEnabled returns whether adaptive closest replica read can be enabled.
// go: IsAdaptiveReplicaReadEnabled L1
pub fn is_adaptive_replica_read_enabled() -> bool {
    enableAdaptiveReplicaRead.load(SeqCst) > 0
}

/// RewritePhaseInfo records some information about the rewrite phase
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RewritePhaseInfo {
    /// DurationRewrite is the duration of rewriting the SQL.
    pub duration_rewrite: i64,
    /// DurationPreprocessSubQuery is the duration of pre-processing sub-queries.
    pub duration_preprocess_sub_query: i64,
    /// PreprocessSubQueries is the number of pre-processed sub-queries.
    pub preprocess_sub_queries: i64,
}

impl RewritePhaseInfo {
    /// Reset resets all fields in RewritePhaseInfo.
    // go: RewritePhaseInfo.Reset L1
    pub fn reset(&mut self) {
        self.duration_rewrite = 0;
        self.duration_preprocess_sub_query = 0;
        self.preprocess_sub_queries = 0;
    }
}

/// ReadConsistencyLevel is the level of read consistency.
pub type ReadConsistencyLevel = str;
/// ReadConsistencyStrict means read by strict consistency, default value.
pub const ReadConsistencyStrict: &ReadConsistencyLevel = "strict";
/// ReadConsistencyWeak means read can be weak consistency.
pub const ReadConsistencyWeak: &ReadConsistencyLevel = "weak";

/// IsWeak returns true only if it's a weak-consistency read.
// go: ReadConsistencyLevel.IsWeak L1
pub fn read_consistency_level_is_weak(r: &ReadConsistencyLevel) -> bool {
    r == ReadConsistencyWeak
}

/// ConnTypeSocket indicates socket without TLS.
pub const ConnTypeSocket: &str = "TCP";
/// ConnTypeUnixSocket indicates Unix Socket.
pub const ConnTypeUnixSocket: &str = "UnixSocket";
/// ConnTypeTLS indicates socket with TLS.
pub const ConnTypeTLS: &str = "SSL/TLS";

/// ConnectionInfo presents the connection information, which is mainly used by audit logs.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ConnectionInfo {
    /// ConnectionID is the connection ID.
    pub connection_id: u64,
    /// ConnectionType is one of `ConnTypeSocket`, `ConnTypeUnixSocket`, `ConnTypeTLS`.
    pub connection_type: String,
    /// Host is the host name of the client.
    pub host: String,
    /// ClientIP is the IP of the client.
    pub client_ip: String,
    /// ClientPort is the port of the client.
    pub client_port: String,
    /// ServerID is the ID of the server.
    pub server_id: i64,
    /// ServerIP is the IP of the server.
    pub server_ip: String,
    /// ServerPort is the port of the server.
    pub server_port: i64,
    /// Duration is the duration of the connection.
    pub duration: f64,
    /// User is the user name.
    pub user: String,
    /// ServerOSLoginUser is the OS user running the server.
    pub server_os_login_user: String,
    /// OSVersion is the OS version of the server.
    pub os_version: String,
    /// ClientVersion is the client version.
    pub client_version: String,
    /// ServerVersion is the server version.
    pub server_version: String,
    /// SSLVersion is the TLS version.
    pub ssl_version: String,
    /// PID is the process ID of the server.
    pub pid: i64,
    /// DB is the current database.
    pub db: String,
    /// AuthMethod is the authentication method.
    pub auth_method: String,
    /// Attributes are the connection attributes sent by the client.
    pub attributes: HashMap<String, String>,
}

impl ConnectionInfo {
    /// IsSecureTransport checks whether the connection is secure.
    // go: ConnectionInfo.IsSecureTransport L1
    pub fn is_secure_transport(&self) -> bool {
        matches!(
            self.connection_type.as_str(),
            ConnTypeUnixSocket | ConnTypeTLS
        )
    }
}

/// String convert Runtime Filter Type to String name
// go: RuntimeFilterType.String L1
pub fn runtime_filter_type_string(rf_type: RuntimeFilterType) -> &'static str {
    match rf_type {
        In => "IN",
        MinMax => "MIN_MAX",
        _ => "",
    }
}

/// String convert Runtime Filter Mode to String name
// go: RuntimeFilterMode.String L1
pub fn runtime_filter_mode_string(rf_mode: RuntimeFilterMode) -> &'static str {
    match rf_mode {
        RFOff => "OFF",
        RFLocal => "LOCAL",
        RFGlobal => "GLOBAL",
        _ => "",
    }
}

/// ToTiPBTiFlashPreAggMode return the corresponding tipb value of preaggregation mode.
// go: ToTiPBTiFlashPreAggMode L1
pub fn to_tipb_tiflash_pre_agg_mode(mode: &str) -> (tipb::TiFlashPreAggMode, bool) {
    match mode {
        ForcePreAggStr => (tipb::TiFlashPreAggMode::ForcePreAgg, true),
        ForceStreamingStr => (tipb::TiFlashPreAggMode::ForceStreaming, true),
        AutoStr => (tipb::TiFlashPreAggMode::Auto, true),
        _ => (tipb::TiFlashPreAggMode::ForcePreAgg, false),
    }
}

/// `planCacheStmtKey`: the key of the session's statement caches.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PlanCacheStmtKey(pub String);

impl PlanCacheStmtKey {
    /// Hash returns the key bytes (`kvcache.Key`).
    // go: planCacheStmtKey.Hash L1
    pub fn hash(&self) -> Vec<u8> {
        self.0.as_bytes().to_vec()
    }
}

/// PrepareDedupCacheKey builds the lookup key for the prepare dedup cache.
/// Including charset, collation, currentDB and sqlMode ensures that the cached
/// PlanCacheStmt is only reused when the session context that affects parsing,
/// name-resolution and cacheability decisions is identical. sqlMode is included
/// because flags like PIPES_AS_CONCAT and ANSI_QUOTES change AST shape, and
/// IsASTCacheable (which computes StmtCacheable) runs on that AST.
// Note: Go string bytes.
// go: PrepareDedupCacheKey L1
pub fn prepare_dedup_cache_key(
    sql: &[u8],
    charset: &str,
    collation: &str,
    current_db: &str,
    sql_mode: tidb_base::mysql::SQLMode,
) -> Vec<u8> {
    let mode_buf = (sql_mode.0 as u64).to_le_bytes();
    let mut key = Vec::with_capacity(sql.len() + charset.len() + collation.len() + 20);
    key.extend_from_slice(sql);
    key.push(0);
    key.extend_from_slice(charset.as_bytes());
    key.push(0);
    key.extend_from_slice(collation.as_bytes());
    key.push(0);
    key.extend_from_slice(current_db.as_bytes());
    key.push(0);
    key.extend_from_slice(&mode_buf);
    key
}

impl SessionVars {
    /// GetSessionVars implements the `SessionVarsProvider` interface.
    // go: SessionVars.GetSessionVars L1
    pub fn get_session_vars(&self) -> &SessionVars {
        self
    }

    /// GetOptimizerFixControlMap returns the specified value of the optimizer fix control.
    // go: SessionVars.GetOptimizerFixControlMap L1
    pub fn get_optimizer_fix_control_map(&self) -> &HashMap<u64, String> {
        &self.state.optimizer_fix_control
    }

    /// AddPlanReplayerFinishedTaskKey record finished task key in session
    // go: SessionVars.AddPlanReplayerFinishedTaskKey L1
    pub fn add_plan_replayer_finished_task_key(&mut self, key: PlanReplayerTaskKey) {
        if self
            .state
            .plan_replayer_finished_task_key
            .as_ref()
            .is_some_and(|m| m.len() >= planReplayerSessionFinishedTaskKeyLen)
        {
            self.initialize_plan_replayer_finished_task_key();
        }
        // Go assigns into a nil map here (a panic) when the map was never initialized; the
        // map is created first instead (no caller adds before checking).
        self.state
            .plan_replayer_finished_task_key
            .get_or_insert_with(HashSet::new)
            .insert(key);
    }

    // go: SessionVars.initializePlanReplayerFinishedTaskKey L1
    fn initialize_plan_replayer_finished_task_key(&mut self) {
        self.state.plan_replayer_finished_task_key = Some(HashSet::with_capacity(
            planReplayerSessionFinishedTaskKeyLen,
        ));
    }

    /// CheckPlanReplayerFinishedTaskKey check whether the key exists
    // go: SessionVars.CheckPlanReplayerFinishedTaskKey L1
    pub fn check_plan_replayer_finished_task_key(&mut self, key: &PlanReplayerTaskKey) -> bool {
        match &self.state.plan_replayer_finished_task_key {
            None => {
                self.initialize_plan_replayer_finished_task_key();
                false
            }
            Some(m) => m.contains(key),
        }
    }

    /// IsPlanReplayerCaptureEnabled indicates whether capture or continues capture enabled
    // go: SessionVars.IsPlanReplayerCaptureEnabled L1
    pub fn is_plan_replayer_capture_enabled(&self) -> bool {
        self.enable_plan_replayer_capture || self.enable_plan_replayed_continues_capture
    }

    /// GetPreparedStmtByName returns the prepared statement specified by stmtName.
    // go: SessionVars.GetPreparedStmtByName L1
    pub fn get_prepared_stmt_by_name(
        &self,
        stmt_name: &str,
    ) -> Result<Arc<dyn Any + Send + Sync>, Error> {
        let Some(stmt_id) = self.state.prepared_stmt_name_to_id.get(stmt_name) else {
            return Err(ErrStmtNotFound.clone());
        };
        self.get_prepared_stmt_by_id(*stmt_id)
    }

    /// GetPreparedStmtByID returns the prepared statement specified by stmtID.
    // go: SessionVars.GetPreparedStmtByID L1
    pub fn get_prepared_stmt_by_id(
        &self,
        stmt_id: u32,
    ) -> Result<Arc<dyn Any + Send + Sync>, Error> {
        match self.state.prepared_stmts.get(&stmt_id) {
            Some(stmt) => Ok(stmt.clone()),
            None => Err(ErrStmtNotFound.clone()),
        }
    }

    /// IsMPPAllowed returns whether mpp execution is allowed.
    // go: SessionVars.IsMPPAllowed L1
    pub fn is_mpp_allowed(&self) -> bool {
        self.allow_mpp_execution
    }

    /// IsTiFlashCopBanned returns whether cop execution is allowed.
    // go: SessionVars.IsTiFlashCopBanned L1
    pub fn is_tiflash_cop_banned(&self) -> bool {
        !self.allow_tiflash_cop
    }

    /// IsMPPEnforced returns whether mpp execution is enforced.
    // go: SessionVars.IsMPPEnforced L1
    pub fn is_mpp_enforced(&self) -> bool {
        self.allow_mpp_execution && self.enforce_mpp_execution
    }

    /// ChooseMppExchangeCompressionMode indicates the data compression method in mpp exchange operator
    // Note: kv.ExchangeCompressionMode as i64, DEF.MPP.
    // go: SessionVars.ChooseMppExchangeCompressionMode L1
    pub fn choose_mpp_exchange_compression_mode(&self) -> i64 {
        if self.mpp_exchange_compression_mode == ExchangeCompressionModeUnspecified {
            // If unspecified, use recommended mode
            return RecommendedExchangeCompressionMode;
        }
        self.mpp_exchange_compression_mode
    }

    /// IsPartialOrderedIndexForTopNEnabled indicates whether partial ordered index optimization for TopN is enabled.
    // go: SessionVars.IsPartialOrderedIndexForTopNEnabled L1
    pub fn is_partial_ordered_index_for_top_n_enabled(&self) -> bool {
        self.opt_partial_ordered_index_for_top_n == "COST"
    }

    /// CheckAndGetTxnScope will return the transaction scope we should use in the current session.
    // go: SessionVars.CheckAndGetTxnScope L1
    pub fn check_and_get_txn_scope(&self) -> String {
        if self.in_restricted_sql || !EnableLocalTxn.load(SeqCst) {
            return GlobalTxnScope.to_owned();
        }
        if self.state.txn_scope.get_var_value() == LocalTxnScope {
            return self.state.txn_scope.get_txn_scope().to_owned();
        }
        GlobalTxnScope.to_owned()
    }

    /// IsDynamicPartitionPruneEnabled indicates whether dynamic partition prune enabled
    /// Note that: IsDynamicPartitionPruneEnabled only indicates whether dynamic partition prune mode is enabled according to
    /// session variable, it isn't guaranteed to be used during query due to other conditions checking.
    // go: SessionVars.IsDynamicPartitionPruneEnabled L1
    pub fn is_dynamic_partition_prune_enabled(&self) -> bool {
        self.state.partition_prune_mode == "dynamic"
    }

    /// IsRowLevelChecksumEnabled indicates whether row level checksum is enabled for current session, that is
    /// tidb_enable_row_level_checksum is on and tidb_row_format_version is 2 and it's not a internal session.
    // go: SessionVars.IsRowLevelChecksumEnabled L1
    pub fn is_row_level_checksum_enabled(&self) -> bool {
        self.enable_row_level_checksum && self.state.row_encoder_enable && !self.in_restricted_sql
    }

    /// AllocNewPlanID alloc new ID
    // go: SessionVars.AllocNewPlanID L1
    pub fn alloc_new_plan_id(&self) -> i64 {
        i64::from(self.state.plan_id.0.fetch_add(1, SeqCst).wrapping_add(1))
    }

    /// SetAllowInSubqToJoinAndAgg set SessionVars.allowInSubqToJoinAndAgg.
    // go: SessionVars.SetAllowInSubqToJoinAndAgg L1
    pub fn set_allow_in_subq_to_join_and_agg(&mut self, val: bool) {
        self.allow_in_subq_to_join_and_agg = val;
    }

    /// GetAllowPreferRangeScan get preferRangeScan from SessionVars.preferRangeScan.
    // go: SessionVars.GetAllowPreferRangeScan L1
    pub fn get_allow_prefer_range_scan(&self) -> bool {
        self.prefer_range_scan
    }

    /// SetAllowPreferRangeScan set SessionVars.preferRangeScan.
    // go: SessionVars.SetAllowPreferRangeScan L1
    pub fn set_allow_prefer_range_scan(&mut self, val: bool) {
        self.prefer_range_scan = val;
    }

    /// SetEnableCascadesPlanner set SessionVars.EnableCascadesPlanner.
    // go: SessionVars.SetEnableCascadesPlanner L1
    pub fn set_enable_cascades_planner(&mut self, val: bool) {
        self.enable_cascades_planner = val;
    }

    /// GetEnableIndexMerge get EnableIndexMerge from SessionVars.enableIndexMerge.
    // go: SessionVars.GetEnableIndexMerge L1
    pub fn get_enable_index_merge(&self) -> bool {
        self.enable_index_merge
    }

    /// SetEnableIndexMerge set SessionVars.enableIndexMerge.
    // go: SessionVars.SetEnableIndexMerge L1
    pub fn set_enable_index_merge(&mut self, val: bool) {
        self.enable_index_merge = val;
    }

    /// GetEnablePseudoForOutdatedStats get EnablePseudoForOutdatedStats from SessionVars.EnablePseudoForOutdatedStats.
    // go: SessionVars.GetEnablePseudoForOutdatedStats L1
    pub fn get_enable_pseudo_for_outdated_stats(&self) -> bool {
        self.enable_pseudo_for_outdated_stats
    }

    /// SetEnablePseudoForOutdatedStats set SessionVars.EnablePseudoForOutdatedStats.
    // go: SessionVars.SetEnablePseudoForOutdatedStats L1
    pub fn set_enable_pseudo_for_outdated_stats(&mut self, val: bool) {
        self.enable_pseudo_for_outdated_stats = val;
    }

    /// IsReplicaReadClosestAdaptive returns whether adaptive closest replica can be enabled.
    // go: SessionVars.IsReplicaReadClosestAdaptive L1
    pub fn is_replica_read_closest_adaptive(&self) -> bool {
        ReplicaReadType(self.replica_read) == ReplicaReadClosestAdaptive
            && is_adaptive_replica_read_enabled()
    }

    /// GetSplitRegionTimeout gets split region timeout.
    // Note: time.Duration as nanoseconds.
    // go: SessionVars.GetSplitRegionTimeout L1
    pub fn get_split_region_timeout(&self) -> i64 {
        (self.wait_split_region_timeout as i64).wrapping_mul(1_000_000_000)
    }

    /// GetIsolationReadEngines gets isolation read engines.
    // go: SessionVars.GetIsolationReadEngines L1
    pub fn get_isolation_read_engines(
        &self,
    ) -> &std::collections::BTreeSet<tidb_base::kv::StoreType> {
        &self.state.isolation_read_engines
    }

    /// AllocPlanColumnID allocates column id for plan.
    // go: SessionVars.AllocPlanColumnID L1
    pub fn alloc_plan_column_id(&self) -> i64 {
        self.state
            .plan_column_id
            .0
            .fetch_add(1, SeqCst)
            .wrapping_add(1)
    }

    /// RegisterScalarSubQ register a scalar sub query into the map. This will be used for EXPLAIN.
    // go: SessionVars.RegisterScalarSubQ L1
    pub fn register_scalar_sub_q(&mut self, scalar_sub_q: Arc<dyn Any + Send + Sync>) {
        self.state.map_scalar_sub_q.push(scalar_sub_q);
    }

    /// GetCharsetInfo gets charset and collation for current context.
    /// What character set should the server translate a statement to after receiving it?
    /// For this, the server uses the character_set_connection and collation_connection system variables.
    /// It converts statements sent by the client from character_set_client to character_set_connection
    /// (except for string literals that have an introducer such as _latin1 or _utf8).
    /// collation_connection is important for comparisons of literal strings.
    /// For comparisons of strings with column values, collation_connection does not matter because columns
    /// have their own collation, which has a higher collation precedence.
    /// See https://dev.mysql.com/doc/refman/5.7/en/charset-connection.html
    // go: SessionVars.GetCharsetInfo L1
    pub fn get_charset_info(&self) -> (String, String) {
        let charset = self
            .systems
            .get(CharacterSetConnection)
            .cloned()
            .unwrap_or_default();
        let collation = self
            .systems
            .get(CollationConnection)
            .cloned()
            .unwrap_or_default();
        (charset, collation)
    }

    /// SetUserVarVal set the value of a user variable (`s.SetUserVarVal`, promoted from the
    /// embedded `*UserVars`).
    // glue: Go promotes UserVars.SetUserVarVal onto SessionVars
    pub fn set_user_var_val(&self, name: &str, dt: Datum) {
        self.state.user_vars.set_user_var_val(name, dt);
    }

    /// SetStringUserVar set the value and collation for user defined variable.
    // go: SessionVars.SetStringUserVar L1
    pub fn set_string_user_var(&self, name: &str, str_val: &[u8], collation: &str) {
        let name = tidb_base::gostd::to_lower(name);
        if !collation.is_empty() {
            self.set_user_var_val(
                &name,
                new_collation_string_datum(str_val.to_vec(), collation.to_owned()),
            );
        } else {
            let (_, collation) = self.get_charset_info();
            self.set_user_var_val(
                &name,
                new_collation_string_datum(str_val.to_vec(), collation),
            );
        }
    }

    /// IsIsolation if true it means the transaction is at that isolation level.
    // go: SessionVars.IsIsolation L1
    pub fn is_isolation(&mut self, isolation: &str) -> bool {
        if !self.state.txn_ctx.isolation.is_empty() {
            return self.state.txn_ctx.isolation == isolation;
        }
        if self.state.txn_isolation_level_one_shot.state == OneShotState::OneShotUse {
            self.state.txn_ctx.isolation = self.state.txn_isolation_level_one_shot.value.clone();
        }
        if self.state.txn_ctx.isolation.is_empty() {
            self.state.txn_ctx.isolation = self.get_system_var(TxnIsolation).0;
        }
        self.state.txn_ctx.isolation == isolation
    }

    /// IsolationLevelForNewTxn returns the isolation level if we want to enter a new transaction
    // go: SessionVars.IsolationLevelForNewTxn L1
    pub fn isolation_level_for_new_txn(&self) -> String {
        let mut isolation = String::new();
        let one_shot = &self.state.txn_isolation_level_one_shot;
        if self.in_txn() {
            if one_shot.state == OneShotState::OneShotSet {
                isolation = one_shot.value.clone();
            }
        } else if one_shot.state == OneShotState::OneShotUse {
            isolation = one_shot.value.clone();
        }

        if isolation.is_empty() {
            isolation = self.get_system_var(TxnIsolation).0;
        }

        isolation
    }

    /// SetTxnIsolationLevelOneShotStateForNextTxn sets the txnIsolationLevelOneShot.state for next transaction.
    // go: SessionVars.SetTxnIsolationLevelOneShotStateForNextTxn L1
    pub fn set_txn_isolation_level_one_shot_state_for_next_txn(&mut self) {
        let iso_level_one_shot = &mut self.state.txn_isolation_level_one_shot;
        if iso_level_one_shot.state != OneShotState::OneShotDef {
            match iso_level_one_shot.state {
                OneShotState::OneShotSet => iso_level_one_shot.state = OneShotState::OneShotUse,
                OneShotState::OneShotUse => {
                    iso_level_one_shot.state = OneShotState::OneShotDef;
                    iso_level_one_shot.value = String::new();
                }
                OneShotState::OneShotDef => {}
            }
        }
    }

    /// IsPessimisticReadConsistency if true it means the statement is in a read consistency pessimistic transaction.
    // go: SessionVars.IsPessimisticReadConsistency L1
    pub fn is_pessimistic_read_consistency(&mut self) -> bool {
        self.state.txn_ctx.is_pessimistic && self.is_isolation(ReadCommitted)
    }

    /// GetNextPreparedStmtID generates and returns the next session scope prepared statement id.
    // go: SessionVars.GetNextPreparedStmtID L1
    pub fn get_next_prepared_stmt_id(&mut self) -> u32 {
        self.prepared_stmt_id = self.prepared_stmt_id.wrapping_add(1);
        self.prepared_stmt_id
    }

    /// SetNextPreparedStmtID sets the next prepared statement id. It's only used in restoring session states.
    // go: SessionVars.SetNextPreparedStmtID L1
    pub fn set_next_prepared_stmt_id(&mut self, prepared_stmt_id: u32) {
        self.prepared_stmt_id = prepared_stmt_id;
    }

    /// AddPreparedStmt adds prepareStmt to current session and count in global.
    // Note: metrics.PreparedStmtGauge: runtime-only.
    // go: SessionVars.AddPreparedStmt L1
    pub fn add_prepared_stmt(
        &mut self,
        stmt_id: u32,
        stmt: Arc<dyn Any + Send + Sync>,
    ) -> Result<(), Error> {
        if !self.state.prepared_stmts.contains_key(&stmt_id) {
            let max_prepared_stmt_count = MaxPreparedStmtCountValue.load(SeqCst);
            let new_prepared_stmt_count = PreparedStmtCount.fetch_add(1, SeqCst) + 1;
            if max_prepared_stmt_count >= 0 && new_prepared_stmt_count > max_prepared_stmt_count {
                PreparedStmtCount.fetch_sub(1, SeqCst);
                return Err(ErrMaxPreparedStmtCountReached
                    .gen_with_stack_by_args(vec![Arg::from(max_prepared_stmt_count)]));
            }
        }
        self.state.prepared_stmts.insert(stmt_id, stmt);
        Ok(())
    }

    /// RemovePreparedStmt removes preparedStmt from current session and decrease count in global.
    // Note: metrics: runtime-only.
    // go: SessionVars.RemovePreparedStmt L1
    pub fn remove_prepared_stmt(&mut self, stmt_id: u32) {
        if self.state.prepared_stmts.remove(&stmt_id).is_none() {
            return;
        }
        PreparedStmtCount.fetch_sub(1, SeqCst);
    }

    /// WithdrawAllPreparedStmt remove all preparedStmt in current session and decrease count in global.
    // Note: metrics: runtime-only.
    // go: SessionVars.WithdrawAllPreparedStmt L1
    pub fn withdraw_all_prepared_stmt(&self) {
        let ps_count = self.state.prepared_stmts.len();
        if ps_count == 0 {
            return;
        }
        PreparedStmtCount.fetch_sub(ps_count as i64, SeqCst);
    }

    /// GetReadableTxnMode returns the session variable TxnMode but rewrites it to "OPTIMISTIC" when it's empty.
    // go: SessionVars.GetReadableTxnMode L1
    pub fn get_readable_txn_mode(&self) -> String {
        if self.txn_mode.is_empty() {
            return Optimistic.to_owned();
        }
        self.txn_mode.clone()
    }

    /// SetPrevStmtDigest sets the digest of the previous statement.
    // go: SessionVars.SetPrevStmtDigest L1
    pub fn set_prev_stmt_digest(&mut self, prev_stmt_digest: &str) {
        self.prev_stmt_digest = prev_stmt_digest.to_owned();
    }

    /// GetPrevStmtDigest returns the digest of the previous statement.
    // go: SessionVars.GetPrevStmtDigest L1
    pub fn get_prev_stmt_digest(&self) -> &str {
        // Because `prevStmt` may be truncated, so it's senseless to normalize it.
        // Even if `prevStmtDigest` is empty but `prevStmt` is not, just return it anyway.
        &self.prev_stmt_digest
    }

    /// GetDivPrecisionIncrement returns the specified value of DivPrecisionIncrement.
    // go: SessionVars.GetDivPrecisionIncrement L1
    pub fn get_div_precision_increment(&self) -> i64 {
        self.div_precision_increment
    }

    /// SetResourceGroupName changes the resource group name and inc/dec the metrics accordingly.
    // Note: metrics.ConnGauge: runtime-only.
    // go: SessionVars.SetResourceGroupName L1
    pub fn set_resource_group_name(&mut self, group_name: &str) {
        self.resource_group_name = group_name.to_owned();
    }

    /// CleanupTxnReadTSIfUsed cleans txnReadTS if used
    // go: SessionVars.CleanupTxnReadTSIfUsed L1
    pub fn cleanup_txn_read_ts_if_used(&mut self) {
        let Some(t) = &self.state.txn_read_ts else {
            return;
        };
        if t.used() && t.peak_txn_read_ts() > 0 {
            self.state.txn_read_ts = Some(TxnReadTS::new(0));
            self.state.snapshot_infoschema = false;
        }
    }

    /// GetCPUFactor returns the session variable cpuFactor
    // go: SessionVars.GetCPUFactor L1
    pub fn get_cpu_factor(&self) -> f64 {
        self.cpu_factor
    }

    /// GetCopCPUFactor returns the session variable copCPUFactor
    // go: SessionVars.GetCopCPUFactor L1
    pub fn get_cop_cpu_factor(&self) -> f64 {
        self.cop_cpu_factor
    }

    /// GetMemoryFactor returns the session variable memoryFactor
    // go: SessionVars.GetMemoryFactor L1
    pub fn get_memory_factor(&self) -> f64 {
        self.memory_factor
    }

    /// GetDiskFactor returns the session variable diskFactor
    // go: SessionVars.GetDiskFactor L1
    pub fn get_disk_factor(&self) -> f64 {
        self.disk_factor
    }

    /// GetConcurrencyFactor returns the session variable concurrencyFactor
    // go: SessionVars.GetConcurrencyFactor L1
    pub fn get_concurrency_factor(&self) -> f64 {
        self.concurrency_factor
    }

    /// GetNetworkFactor returns the session variable networkFactor
    /// returns 0 when tbl is a temporary table.
    // go: SessionVars.GetNetworkFactor L1
    pub fn get_network_factor(&self, tbl: Option<&TableInfo>) -> f64 {
        if tbl.is_some_and(|tbl| tbl.temp_table_type != TempTableNone) {
            return 0.0;
        }
        self.network_factor
    }

    /// GetScanFactor returns the session variable scanFactor
    /// returns 0 when tbl is a temporary table.
    // go: SessionVars.GetScanFactor L1
    pub fn get_scan_factor(&self, tbl: Option<&TableInfo>) -> f64 {
        if tbl.is_some_and(|tbl| tbl.temp_table_type != TempTableNone) {
            return 0.0;
        }
        self.scan_factor
    }

    /// GetDescScanFactor returns the session variable descScanFactor
    /// returns 0 when tbl is a temporary table.
    // go: SessionVars.GetDescScanFactor L1
    pub fn get_desc_scan_factor(&self, tbl: Option<&TableInfo>) -> f64 {
        if tbl.is_some_and(|tbl| tbl.temp_table_type != TempTableNone) {
            return 0.0;
        }
        self.desc_scan_factor
    }

    /// GetSeekFactor returns the session variable seekFactor
    /// returns 0 when tbl is a temporary table.
    // go: SessionVars.GetSeekFactor L1
    pub fn get_seek_factor(&self, tbl: Option<&TableInfo>) -> f64 {
        if tbl.is_some_and(|tbl| tbl.temp_table_type != TempTableNone) {
            return 0.0;
        }
        self.seek_factor
    }

    /// EnableEvalTopNEstimationForStrMatch means if we need to evaluate expression with TopN to improve estimation.
    /// Currently, it's only for string matching functions (like and regexp).
    // go: SessionVars.EnableEvalTopNEstimationForStrMatch L1
    pub fn enable_eval_top_n_estimation_for_str_match(&self) -> bool {
        self.default_str_match_selectivity == 0.0
    }

    /// GetStrMatchDefaultSelectivity means the default selectivity for like and regexp.
    /// Note: 0 is a special value, which means the default selectivity is 0.1 and TopN assisted estimation is enabled.
    // go: SessionVars.GetStrMatchDefaultSelectivity L1
    pub fn get_str_match_default_selectivity(&self) -> f64 {
        if self.default_str_match_selectivity == 0.0 {
            return 0.1;
        }
        self.default_str_match_selectivity
    }

    /// GetNegateStrMatchDefaultSelectivity means the default selectivity for not like and not regexp.
    /// Note:
    ///
    /// - 0 is a special value, which means the default selectivity is 0.9 and TopN assisted estimation is enabled.
    /// - 0.8 (the default value) is also a special value. For backward compatibility, when the variable is set to 0.8, we
    ///   keep the default selectivity of like/regexp and not like/regexp all 0.8.
    // go: SessionVars.GetNegateStrMatchDefaultSelectivity L1
    pub fn get_negate_str_match_default_selectivity(&self) -> f64 {
        if self.default_str_match_selectivity == DefTiDBDefaultStrMatchSelectivity {
            return DefTiDBDefaultStrMatchSelectivity;
        }
        1.0 - self.get_str_match_default_selectivity()
    }

    /// GetRelatedTableForMDL gets the related table for metadata lock.
    // go: SessionVars.GetRelatedTableForMDL L1
    pub fn get_related_table_for_mdl(&mut self) -> super::session_state::RelatedTableForMDL {
        self.state
            .txn_ctx
            .related_table_for_mdl
            .get_or_insert_with(Default::default)
            .clone()
    }

    /// ClearRelatedTableForMDL clears the related table for MDL.
    /// related tables for MDL is filled during build logical plan or Preprocess for all DataSources,
    /// even for queries inside DDLs like `create view as select xxx` and `create table as select xxx`.
    /// it should be cleared before we execute the DDL statement.
    // go: SessionVars.ClearRelatedTableForMDL L1
    pub fn clear_related_table_for_mdl(&mut self) {
        self.state.txn_ctx.related_table_for_mdl = None;
    }

    /// EnableForceInlineCTE returns the session variable enableForceInlineCTE
    // go: SessionVars.EnableForceInlineCTE L1
    pub fn enable_force_inline_cte(&self) -> bool {
        self.fields.enable_force_inline_cte
    }

    /// IsRuntimeFilterEnabled return runtime filter mode whether OFF
    // go: SessionVars.IsRuntimeFilterEnabled L1
    pub fn is_runtime_filter_enabled(&self) -> bool {
        self.runtime_filter_mode != RFOff
    }

    /// GetRuntimeFilterTypes return the session variable runtimeFilterTypes
    // go: SessionVars.GetRuntimeFilterTypes L1
    pub fn get_runtime_filter_types(&self) -> &[RuntimeFilterType] {
        &self.state.runtime_filter_types
    }

    /// GetRuntimeFilterMode return the session variable runtimeFilterMode
    // go: SessionVars.GetRuntimeFilterMode L1
    pub fn get_runtime_filter_mode(&self) -> RuntimeFilterMode {
        self.runtime_filter_mode
    }

    /// GetTiKVClientReadTimeout returns readonly kv request timeout, prefer query hint over session variable
    // go: SessionVars.GetTiKVClientReadTimeout L1
    pub fn get_tikv_client_read_timeout(&self) -> u64 {
        self.tikv_client_read_timeout
    }

    /// GetOptObjective return the session variable "tidb_opt_objective".
    /// Please see comments of SessionVars.OptObjective for details.
    // go: SessionVars.GetOptObjective L1
    pub fn get_opt_objective(&self) -> &str {
        &self.opt_objective
    }

    /// UseLowResolutionTSO indicates whether low resolution tso could be used for execution.
    /// After `tidb_low_resolution_tso` supports the global scope, this variable is expected to only affect
    /// user sessions and not impact internal background sessions and tasks.
    /// Currently, one of the problems is that the determination of whether a session is an internal task
    /// session within TiDB is quite inconsistent and chaotic, posing risks. Some internal sessions rely on
    /// upper-level users correctly using `ExecuteInternal` or `ExecuteRestrictedSQL` for assurance.
    /// Additionally, the BR code also contains some session-related encapsulation and usage.
    ///
    /// TODO: There needs to be a more comprehensive and unified entry point to ensure that all internal
    /// sessions and global user sessions/variables are isolated and do not affect each other.
    // go: SessionVars.UseLowResolutionTSO L1
    pub fn use_low_resolution_tso(&self) -> bool {
        !self.in_restricted_sql && self.low_resolution_tso && self.connection_id > 0
    }
}
