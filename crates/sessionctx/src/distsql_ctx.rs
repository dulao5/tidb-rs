//! go-file: pkg/distsql/context/context.go
//!
//! Port of Go TiDB v8.5.8 `pkg/distsql/context/context.go` (block D00.1, P1-1 handoff to D07.3):
//! `DistSQLContext`, `AppendWarning`, `Detach`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use tidb_base::Error;
use tidb_base::contextutil::WarnAppenderRef;
use tidb_base::gostd::time::{Location, utc};
use tidb_base::mysql::PriorityEnum;
use tidb_execdetails::execdetails::{RuntimeStatsColl, SyncExecDetails};
use tidb_execdetails::ppcpuusage::SQLCPUUsages;
use tidb_kv::copr::{ResourceGroupTagBuilder, Variables};
use tidb_kv::kv::Client;
use tidb_kv::option::{ReplicaReadLeader, ReplicaReadType};
use tidb_memory::sqlkiller::SqlKiller;
use tidb_memory::tracker::Tracker;

use crate::errctx;

/// DistSQLContext provides all information needed by using functions in `distsql`.
///
/// Go's `KvExecCounter` (D14.2 Top SQL, omitted per ROADMAP §3) and the 7 `TiFlash*` fields
/// (`DEF.MPP`) are omitted on the read-only TiKV path (D07.3 spec §4.2).
/// `has_runaway_rule` replaces Go's `RunawayChecker` until N10 is decided: when `true`,
/// `RequestBuilder::set_from_session_vars` returns `blocked("N10", ...)`.
pub struct DistSQLContext {
    pub warn_handler: Option<WarnAppenderRef>,

    pub in_restricted_sql: bool,
    pub client: Option<Arc<dyn Client>>,

    pub enabled_rate_limit_action: bool,
    pub enable_chunk_rpc: bool,
    pub original_sql: String,
    pub kv_vars: Variables,
    pub session_mem_tracker: Option<Arc<Tracker>>,

    pub location: Arc<Location>,
    pub runtime_stats_coll: Option<Arc<RuntimeStatsColl>>,
    pub sql_killer: Option<Arc<SqlKiller>>,
    pub cpu_usage: Arc<SQLCPUUsages>,
    pub err_ctx: errctx::Context,

    pub dist_sql_concurrency: isize,
    pub replica_read_type: ReplicaReadType,
    pub weak_consistency: bool,
    pub rc_check_ts: bool,
    pub not_fill_cache: bool,
    pub task_id: u64,
    pub priority: PriorityEnum,
    pub resource_group_tagger: Option<Arc<ResourceGroupTagBuilder>>,
    pub enable_paging: bool,
    pub min_paging_size: isize,
    pub max_paging_size: isize,
    pub request_source_type: String,
    pub explicit_request_source_type: String,
    pub store_batch_size: isize,
    pub resource_group_name: String,
    pub load_based_replica_read_threshold: Duration,
    /// True when the session's bound resource group configures a runaway rule (N10 fence).
    pub has_runaway_rule: bool,
    pub tikv_client_read_timeout: u64,

    pub replica_closest_read_threshold: i64,
    pub connection_id: u64,
    pub session_alias: String,

    pub exec_details: Arc<SyncExecDetails>,

    /// Only one cop-reader can use lite worker at the same time. Using lite-worker in
    /// multiple readers will affect the concurrent execution of readers.
    pub try_cop_lite_worker: Arc<AtomicU32>,
}

impl Default for DistSQLContext {
    // glue: zero-value DistSQLContext for tests and partial construction
    fn default() -> Self {
        DistSQLContext {
            warn_handler: None,
            in_restricted_sql: false,
            client: None,
            enabled_rate_limit_action: false,
            enable_chunk_rpc: false,
            original_sql: String::new(),
            kv_vars: Variables::default(),
            session_mem_tracker: None,
            location: utc(),
            runtime_stats_coll: None,
            sql_killer: None,
            cpu_usage: Arc::new(SQLCPUUsages::default()),
            err_ctx: errctx::StrictNoWarningContext.clone(),
            dist_sql_concurrency: 0,
            replica_read_type: ReplicaReadLeader,
            weak_consistency: false,
            rc_check_ts: false,
            not_fill_cache: false,
            task_id: 0,
            priority: PriorityEnum::default(),
            resource_group_tagger: None,
            enable_paging: false,
            min_paging_size: 0,
            max_paging_size: 0,
            request_source_type: String::new(),
            explicit_request_source_type: String::new(),
            store_batch_size: 0,
            resource_group_name: String::new(),
            load_based_replica_read_threshold: Duration::ZERO,
            has_runaway_rule: false,
            tikv_client_read_timeout: 0,
            replica_closest_read_threshold: 0,
            connection_id: 0,
            session_alias: String::new(),
            exec_details: Arc::new(SyncExecDetails::default()),
            try_cop_lite_worker: Arc::new(AtomicU32::new(0)),
        }
    }
}

impl std::fmt::Debug for DistSQLContext {
    // glue: Debug for a struct holding trait objects
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DistSQLContext")
            .field("in_restricted_sql", &self.in_restricted_sql)
            .field("has_client", &self.client.is_some())
            .field("enable_chunk_rpc", &self.enable_chunk_rpc)
            .field("dist_sql_concurrency", &self.dist_sql_concurrency)
            .field("task_id", &self.task_id)
            .field("resource_group_name", &self.resource_group_name)
            .field("has_runaway_rule", &self.has_runaway_rule)
            .finish_non_exhaustive()
    }
}

impl DistSQLContext {
    /// AppendWarning appends the warning to the warning handler.
    // go: DistSQLContext.AppendWarning L1
    pub fn append_warning(&self, warn: Error) {
        if let Some(h) = &self.warn_handler {
            h.append_warning(warn);
        }
    }

    /// Detach detaches this context from the session context.
    ///
    /// NOTE: Though this session context can be used parallelly with this context after
    /// calling it, the `StatementContext` cannot. The session context should create a new
    /// `StatementContext` before executing another statement.
    // go: DistSQLContext.Detach L1
    pub fn detach(&self) -> DistSQLContext {
        let new_cpu_usages = Arc::new(SQLCPUUsages::default());
        new_cpu_usages.set_cpu_usages(self.cpu_usage.get_cpu_usages());
        let mut new_kv_vars = self.kv_vars.clone();
        if let Some(killer) = &self.sql_killer {
            new_kv_vars.killed = Some(killer.signal_handle());
        }
        DistSQLContext {
            warn_handler: self.warn_handler.clone(),
            in_restricted_sql: self.in_restricted_sql,
            client: self.client.clone(),
            enabled_rate_limit_action: self.enabled_rate_limit_action,
            enable_chunk_rpc: self.enable_chunk_rpc,
            original_sql: self.original_sql.clone(),
            kv_vars: new_kv_vars,
            session_mem_tracker: self.session_mem_tracker.clone(),
            location: Arc::clone(&self.location),
            runtime_stats_coll: self.runtime_stats_coll.clone(),
            sql_killer: self.sql_killer.clone(),
            cpu_usage: new_cpu_usages,
            err_ctx: self.err_ctx.clone(),
            dist_sql_concurrency: self.dist_sql_concurrency,
            replica_read_type: self.replica_read_type,
            weak_consistency: self.weak_consistency,
            rc_check_ts: self.rc_check_ts,
            not_fill_cache: self.not_fill_cache,
            task_id: self.task_id,
            priority: self.priority,
            resource_group_tagger: self.resource_group_tagger.clone(),
            enable_paging: self.enable_paging,
            min_paging_size: self.min_paging_size,
            max_paging_size: self.max_paging_size,
            request_source_type: self.request_source_type.clone(),
            explicit_request_source_type: self.explicit_request_source_type.clone(),
            store_batch_size: self.store_batch_size,
            resource_group_name: self.resource_group_name.clone(),
            load_based_replica_read_threshold: self.load_based_replica_read_threshold,
            has_runaway_rule: self.has_runaway_rule,
            tikv_client_read_timeout: self.tikv_client_read_timeout,
            replica_closest_read_threshold: self.replica_closest_read_threshold,
            connection_id: self.connection_id,
            session_alias: self.session_alias.clone(),
            exec_details: Arc::clone(&self.exec_details),
            try_cop_lite_worker: Arc::new(AtomicU32::new(
                self.try_cop_lite_worker.load(Ordering::Relaxed),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tidb_base::contextutil::{WarnHandler, new_static_warn_handler};
    use tidb_base::mysql::HighPriority;
    use tidb_execdetails::ppcpuusage::CPUUsages;
    use tidb_kv::option::ReplicaReadFollower;

    // Port of Go `pkg/distsql/context/context_test.go:TestContextDetach`.
    #[test]
    fn test_context_detach() {
        let killer = Arc::new(SqlKiller::default());
        killer.send_kill_signal(1);
        let cpu = Arc::new(SQLCPUUsages::default());
        cpu.set_cpu_usages(CPUUsages {
            tidb_cpu_time: Duration::from_millis(7),
            tikv_cpu_time: Duration::from_millis(13),
        });
        let kv_vars = Variables {
            backoff_weight: 2,
            killed: Some(killer.signal_handle()),
        };
        let warn_handler = Arc::new(new_static_warn_handler(5));

        let obj = DistSQLContext {
            warn_handler: Some(warn_handler.clone()),
            in_restricted_sql: true,
            client: None,
            enabled_rate_limit_action: true,
            enable_chunk_rpc: true,
            original_sql: "a".to_owned(),
            kv_vars,
            session_mem_tracker: Some(Arc::new(Tracker::default())),
            location: utc(),
            runtime_stats_coll: Some(Arc::new(RuntimeStatsColl::default())),
            sql_killer: Some(Arc::clone(&killer)),
            cpu_usage: Arc::clone(&cpu),
            err_ctx: errctx::new_context_with_levels([errctx::LevelWarn; 7], warn_handler.clone()),
            dist_sql_concurrency: 1,
            replica_read_type: ReplicaReadFollower,
            weak_consistency: true,
            rc_check_ts: true,
            not_fill_cache: true,
            task_id: 1,
            priority: HighPriority,
            resource_group_tagger: Some(Arc::new(tidb_kv::copr::new_resource_group_tag_builder())),
            enable_paging: true,
            min_paging_size: 1,
            max_paging_size: 1,
            request_source_type: "a".to_owned(),
            explicit_request_source_type: "b".to_owned(),
            store_batch_size: 1,
            resource_group_name: "c".to_owned(),
            load_based_replica_read_threshold: Duration::from_secs(1),
            has_runaway_rule: true,
            tikv_client_read_timeout: 1,
            replica_closest_read_threshold: 1,
            connection_id: 1,
            session_alias: "c".to_owned(),
            exec_details: Arc::new(SyncExecDetails::default()),
            try_cop_lite_worker: Arc::new(AtomicU32::new(1)),
        };

        obj.append_warning(Error::plain("test warning"));
        assert_eq!(warn_handler.warning_count(), 1);

        let detached = obj.detach();

        // Scalar fields preserved.
        assert_eq!(detached.in_restricted_sql, obj.in_restricted_sql);
        assert_eq!(
            detached.enabled_rate_limit_action,
            obj.enabled_rate_limit_action
        );
        assert_eq!(detached.enable_chunk_rpc, obj.enable_chunk_rpc);
        assert_eq!(detached.original_sql, obj.original_sql);
        assert_eq!(detached.dist_sql_concurrency, obj.dist_sql_concurrency);
        assert_eq!(detached.replica_read_type, obj.replica_read_type);
        assert_eq!(detached.weak_consistency, obj.weak_consistency);
        assert_eq!(detached.rc_check_ts, obj.rc_check_ts);
        assert_eq!(detached.not_fill_cache, obj.not_fill_cache);
        assert_eq!(detached.task_id, obj.task_id);
        assert_eq!(detached.priority, obj.priority);
        assert_eq!(detached.enable_paging, obj.enable_paging);
        assert_eq!(detached.min_paging_size, obj.min_paging_size);
        assert_eq!(detached.max_paging_size, obj.max_paging_size);
        assert_eq!(detached.request_source_type, obj.request_source_type);
        assert_eq!(
            detached.explicit_request_source_type,
            obj.explicit_request_source_type
        );
        assert_eq!(detached.store_batch_size, obj.store_batch_size);
        assert_eq!(detached.resource_group_name, obj.resource_group_name);
        assert_eq!(
            detached.load_based_replica_read_threshold,
            obj.load_based_replica_read_threshold
        );
        assert_eq!(detached.has_runaway_rule, obj.has_runaway_rule);
        assert_eq!(
            detached.tikv_client_read_timeout,
            obj.tikv_client_read_timeout
        );
        assert_eq!(
            detached.replica_closest_read_threshold,
            obj.replica_closest_read_threshold
        );
        assert_eq!(detached.connection_id, obj.connection_id);
        assert_eq!(detached.session_alias, obj.session_alias);

        // Shared Arc handles point to the same instances as Go pointer-compared fields.
        assert!(Arc::ptr_eq(
            detached.sql_killer.as_ref().unwrap(),
            obj.sql_killer.as_ref().unwrap()
        ));
        assert!(Arc::ptr_eq(&detached.exec_details, &obj.exec_details));
        assert!(Arc::ptr_eq(
            detached.runtime_stats_coll.as_ref().unwrap(),
            obj.runtime_stats_coll.as_ref().unwrap()
        ));
        assert!(Arc::ptr_eq(
            detached.session_mem_tracker.as_ref().unwrap(),
            obj.session_mem_tracker.as_ref().unwrap()
        ));
        assert!(Arc::ptr_eq(
            detached.kv_vars.killed.as_ref().unwrap(),
            obj.kv_vars.killed.as_ref().unwrap()
        ));

        // Detached copies have independent storage with equal initial values.
        assert!(!Arc::ptr_eq(&detached.cpu_usage, &obj.cpu_usage));
        assert_eq!(
            detached.cpu_usage.get_cpu_usages(),
            obj.cpu_usage.get_cpu_usages()
        );
        detached
            .cpu_usage
            .merge_tikv_cpu_time(Duration::from_millis(5));
        assert_ne!(
            detached.cpu_usage.get_cpu_usages(),
            obj.cpu_usage.get_cpu_usages()
        );

        assert!(!Arc::ptr_eq(
            &detached.try_cop_lite_worker,
            &obj.try_cop_lite_worker
        ));
        assert_eq!(detached.try_cop_lite_worker.load(Ordering::Relaxed), 1);
        detached.try_cop_lite_worker.store(2, Ordering::Relaxed);
        assert_eq!(obj.try_cop_lite_worker.load(Ordering::Relaxed), 1);
    }
}
