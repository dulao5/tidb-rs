//! go-file: pkg/util/processinfo.go
//!
//! `ProcessInfo`, `OOMAlarmVariablesInfo`, and `SessionManager` (block D00.3, placed in
//! `tidb-sessionctx` rank 7 via `N0` interface inversion so planner/executor/session/server
//! share them without crate cycles).

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::SystemTime;

use tidb_base::Error;
use tidb_base::mysql::{
    Command2Str, ServerMoreResultsExists, ServerPSOutParams, ServerStatusAutocommit,
    ServerStatusCursorExists, ServerStatusDBDropped, ServerStatusInTrans, ServerStatusLastRowSend,
    ServerStatusMetadataChanged, ServerStatusNoBackslashEscaped, ServerStatusNoGoodIndexUsed,
    ServerStatusNoIndexUsed, ServerStatusWasSlow,
};
use tidb_execdetails::execdetails::RuntimeStatsColl;
use tidb_execdetails::ppcpuusage::SQLCPUUsages;
use tidb_memory::tracker::Tracker;

use crate::stmtctx::StatementContext;

/// OOMAlarmVariablesInfo is a struct for OOM alarm variables.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OOMAlarmVariablesInfo {
    pub session_analyze_version: isize,
    pub session_enabled_rate_limit_action: bool,
    pub session_mem_quota_query: i64,
}

/// ProcessInfo is a struct used for show processlist, kill, and expensive-query monitoring.
#[derive(Clone)]
pub struct ProcessInfo {
    pub id: u64,
    pub user: String,
    pub host: String,
    pub port: String,
    pub db: String,
    pub command: u8,
    pub time: SystemTime,
    pub state: u16,
    pub info: String,
    pub digest: String,
    /// MaxExecutionTime is the timeout for select statement, in milliseconds.
    pub max_execution_time: u64,
    pub redact_sql: String,
    pub session_alias: String,
    pub resource_group_name: String,
    pub cur_txn_start_ts: u64,
    pub cur_txn_create_time: Option<SystemTime>,
    pub expensive_log_time: Option<SystemTime>,
    pub expensive_txn_log_time: Option<SystemTime>,
    pub stmt_ctx: Option<Arc<StatementContext>>,
    pub mem_tracker: Option<Arc<Tracker>>,
    pub disk_tracker: Option<Arc<Tracker>>,
    pub sql_cpu_usage: Option<Arc<SQLCPUUsages>>,
    pub runtime_stats_coll: Option<Arc<RuntimeStatsColl>>,
    pub table_ids: Vec<i64>,
    pub index_names: Vec<String>,
    pub stats_info: String,
    pub oom_alarm_variables_info: OOMAlarmVariablesInfo,
}

impl Default for ProcessInfo {
    // glue: Go zero-value ProcessInfo
    fn default() -> Self {
        Self {
            id: 0,
            user: String::new(),
            host: String::new(),
            port: String::new(),
            db: String::new(),
            command: tidb_base::mysql::ComSleep,
            time: SystemTime::UNIX_EPOCH,
            state: 0,
            info: String::new(),
            digest: String::new(),
            max_execution_time: 0,
            redact_sql: String::new(),
            session_alias: String::new(),
            resource_group_name: String::new(),
            cur_txn_start_ts: 0,
            cur_txn_create_time: None,
            expensive_log_time: None,
            expensive_txn_log_time: None,
            stmt_ctx: None,
            mem_tracker: None,
            disk_tracker: None,
            sql_cpu_usage: None,
            runtime_stats_coll: None,
            table_ids: Vec::new(),
            index_names: Vec::new(),
            stats_info: String::new(),
            oom_alarm_variables_info: OOMAlarmVariablesInfo::default(),
        }
    }
}

impl fmt::Debug for ProcessInfo {
    // glue: Debug for ProcessInfo
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProcessInfo")
            .field("id", &self.id)
            .field("user", &self.user)
            .field("host", &self.host)
            .field("db", &self.db)
            .field("command", &self.command)
            .field("max_execution_time", &self.max_execution_time)
            .field("info", &self.info)
            .finish_non_exhaustive()
    }
}

const ASC_SERVER_STATUS: &[(u16, &str)] = &[
    (ServerStatusInTrans, "in transaction"),
    (ServerStatusAutocommit, "autocommit"),
    (ServerMoreResultsExists, "more results exists"),
    (ServerStatusNoGoodIndexUsed, "no good index used"),
    (ServerStatusNoIndexUsed, "no index used"),
    (ServerStatusCursorExists, "cursor exists"),
    (ServerStatusLastRowSend, "last row send"),
    (ServerStatusDBDropped, "db dropped"),
    (ServerStatusNoBackslashEscaped, "no backslash escaped"),
    (ServerStatusMetadataChanged, "metadata changed"),
    (ServerStatusWasSlow, "was slow"),
    (ServerPSOutParams, "ps out params"),
];

/// Converts server status bitmask to Go's semicolon-separated description.
// go: serverStatus2Str L1
pub fn server_status_to_str(state: u16) -> String {
    let mut parts = Vec::new();
    for &(flag, name) in ASC_SERVER_STATUS {
        if state & flag != 0 {
            parts.push(name);
        }
    }
    parts.join("; ")
}

impl ProcessInfo {
    /// Formats host[:port] matching Go's `net.JoinHostPort(pi.Host, pi.Port)`.
    // go-part: ProcessInfo.ToRowForShow L1 (host formatting)
    pub fn formatted_host(&self) -> String {
        if self.port.is_empty() {
            return self.host.clone();
        }
        if self.host.contains(':') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    /// Formats this `ProcessInfo` identically to Go's `ProcessInfo.String()` for log parity.
    // go: ProcessInfo.String L1
    pub fn summary_string(&self) -> String {
        let elapsed_secs = SystemTime::now()
            .duration_since(self.time)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let db = if self.db.is_empty() {
            "<nil>"
        } else {
            self.db.as_str()
        };
        let cmd = Command2Str.get(&self.command).copied().unwrap_or("<nil>");
        let info = if self.info.is_empty() {
            "<nil>".to_owned()
        } else {
            self.info.chars().take(100).collect()
        };
        let state = server_status_to_str(self.state);
        format!(
            "{{id:{}, user:{}, host:{}, db:{}, command:{}, time:{}, state:{}, info:{}}}",
            self.id,
            self.user,
            self.formatted_host(),
            db,
            cmd,
            elapsed_secs,
            state,
            info
        )
    }
}

impl fmt::Display for ProcessInfo {
    // glue: fmt.Stringer -> ProcessInfo.String
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.summary_string())
    }
}

/// Future returned by [`SessionManager::kill_remote_conn`].
pub type KillRemoteFuture<'a> = Pin<Box<dyn Future<Output = Result<(), Error>> + Send + 'a>>;

/// Session management interface used by `KILL`, `max_execution_time`, `ServerMemoryLimit`,
/// and `ExpensiveQuery` monitoring (`N0` interface inversion at rank 7).
// go-part: SessionManager L2 (omits internal-session/TLS/DDL helpers not yet wired here)
pub trait SessionManager: Send + Sync {
    // glue: interface method util.SessionManager.ShowProcessList
    fn show_process_list(&self) -> HashMap<u64, Arc<ProcessInfo>>;

    // glue: interface method util.SessionManager.GetProcessInfo
    fn get_process_info(&self, id: u64) -> Option<Arc<ProcessInfo>>;

    // glue: interface method util.SessionManager.Kill
    fn kill(&self, connection_id: u64, query: bool, max_execution_time: bool, runaway: bool);

    // glue: interface method util.SessionManager.KillAllConnections
    fn kill_all_connections(&self);

    // glue: interface method util.SessionManager.ServerID
    fn server_id(&self) -> u64;

    /// Returns `true` when this node's `server_id` is uniquely owned and safe for matching
    /// local global connection IDs (`true` for standalone `server_id = 1`; in cluster mode,
    /// `true` only while `/tidb/server_id/<id>` lease is validly held).
    // glue: cluster /tidb/server_id safety guard against cross-node mis-kill
    fn has_valid_server_id(&self) -> bool {
        true
    }

    /// Sends a remote `KILL` RPC (`ExecType::TypeKill`) to `target_server_id`.
    // glue: remote kill RPC hook (SimpleExec::killRemoteConn)
    fn kill_remote_conn(
        &self,
        _target_server_id: u64,
        _conn_id: u64,
        _query: bool,
    ) -> KillRemoteFuture<'_> {
        Box::pin(async {
            Err(tidb_base::errors::new(
                "remote kill RPC is unavailable".to_owned(),
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_info_string_matches_go_format() {
        let mut pi = ProcessInfo {
            id: 2097154,
            user: "root".to_owned(),
            host: "127.0.0.1".to_owned(),
            command: tidb_base::mysql::ComQuery,
            time: SystemTime::now(),
            state: ServerStatusAutocommit,
            info: "SELECT 1".to_owned(),
            ..Default::default()
        };
        assert_eq!(
            pi.to_string(),
            "{id:2097154, user:root, host:127.0.0.1, db:<nil>, command:Query, time:0, state:autocommit, info:SELECT 1}"
        );

        pi.db = "test".to_owned();
        pi.port = "45678".to_owned();
        pi.info.clear();
        pi.command = tidb_base::mysql::ComSleep;
        assert_eq!(
            pi.to_string(),
            "{id:2097154, user:root, host:127.0.0.1:45678, db:test, command:Sleep, time:0, state:autocommit, info:<nil>}"
        );
    }
}
