//! The part of `SessionVars` (pkg/sessionctx/variable/session.go, block D05.2) and of
//! `stmtctx.StatementContext` (D05.2) that the D05.1 validation code reads or writes.
//!
//! glue: D05.2 stub. Block D05.2 replaces this file with the faithful `SessionVars` (N2 /
//! N8 §3.2); the fields here keep Go's names and meaning so the validation hooks do not
//! change when it does. Only what the validation hooks touch is here.

#![allow(non_upper_case_globals)]

use std::collections::HashMap;
use std::sync::Arc;

use tidb_base::Error;
use tidb_base::gostd::time::Location;

use super::names::{ErrorCount, WarningCount};

/// `contextutil.SQLWarn` level names.
pub const WarnLevelWarning: &str = "Warning";

/// `contextutil.SQLWarn`: a warning appended to the statement context.
#[derive(Clone, Debug)]
pub struct SQLWarn {
    /// Level of the warning.
    pub level: &'static str,
    /// The warning.
    pub err: Error,
}

/// The warnings and the statement type of `stmtctx.StatementContext`.
// glue: D05.2 stub of stmtctx.StatementContext
#[derive(Clone, Debug, Default)]
pub struct StatementContext {
    /// StmtType is the statement type (`"Set"` for SET statements).
    pub stmt_type: String,
    warnings: Vec<SQLWarn>,
}

impl StatementContext {
    /// AppendWarning appends a warning with level 'Warning'.
    // glue: D05.2 stub of StatementContext.AppendWarning
    pub fn append_warning(&mut self, err: Error) {
        self.warnings.push(SQLWarn { level: WarnLevelWarning, err });
    }

    /// GetWarnings gets warnings.
    // glue: D05.2 stub of StatementContext.GetWarnings
    pub fn get_warnings(&self) -> Vec<SQLWarn> {
        self.warnings.clone()
    }

    /// SetWarnings sets warnings.
    // glue: D05.2 stub of StatementContext.SetWarnings
    pub fn set_warnings(&mut self, warns: Vec<SQLWarn>) {
        self.warnings = warns;
    }
}

/// GlobalVarAccessor is the interface for accessing global scope system and status variables.
// go: GlobalVarAccessor L2 (read-path methods only; synchronous, like Go)
pub trait GlobalVarAccessor: Send {
    /// GetGlobalSysVar gets the global system variable value for name.
    fn get_global_sys_var(&self, name: &str) -> Result<String, Error>;
    /// SetGlobalSysVarOnly sets the global system variable without calling the validation
    /// function or updating aliases.
    fn set_global_sys_var_only(
        &mut self,
        name: &str,
        value: &str,
        update_local: bool,
    ) -> Result<(), Error>;
    /// GetTiDBTableValue gets a value from mysql.tidb for the key 'name'
    fn get_tidb_table_value(&self, name: &str) -> Result<String, Error>;
}

/// The fields of `SessionVars` the D05.1 validation hooks use.
// glue: D05.2 stub of SessionVars
pub struct SessionVars {
    /// StmtCtx holds variables for current executing statement.
    pub stmt_ctx: StatementContext,
    /// GlobalVarsAccessor is used to set and get global variables.
    pub global_vars_accessor: Box<dyn GlobalVarAccessor>,
    /// systems variables, don't modify it directly, use GetSystemVar/SetSystemVar method.
    pub(crate) systems: HashMap<String, String>,
    /// SysWarningCount is the system variable "warning_count".
    pub sys_warning_count: usize,
    /// SysErrorCount is the system variable "error_count".
    pub sys_error_count: u16,
    /// NoopFuncsMode allows OFF/ON/WARN values as 0/1/2.
    pub noop_funcs_mode: i32,
    /// allowMPPExecution means if we should use mpp way to execute query.
    pub(crate) allow_mpp_execution: bool,
    /// ForeignKeyChecks indicates whether to enable foreign key constraint check.
    pub foreign_key_checks: bool,
    /// TimeZone is the time zone of the session (`nil`: the system location).
    pub time_zone: Option<Arc<Location>>,
    /// TLSConnectionState is the TLS state of the connection (only its presence is read here).
    pub tls_connection_state: Option<()>,
}

impl SessionVars {
    /// A session with Go's `NewSessionVars` defaults for the fields above.
    // glue: D05.2 stub of NewSessionVars
    pub fn new(global_vars_accessor: Box<dyn GlobalVarAccessor>) -> SessionVars {
        SessionVars {
            stmt_ctx: StatementContext::default(),
            global_vars_accessor,
            systems: HashMap::new(),
            sys_warning_count: 0,
            sys_error_count: 0,
            noop_funcs_mode: super::varsutil::tidb_opt_on_off_warn(
                super::tidb_vars::DefTiDBEnableNoopFuncs,
            ),
            allow_mpp_execution: super::tidb_vars::DefTiDBAllowMPPExecution,
            foreign_key_checks: super::tidb_vars::DefTiDBForeignKeyChecks,
            time_zone: None,
            tls_connection_state: None,
        }
    }

    /// GetSystemVar gets the string value of a system variable.
    // glue: D05.2 stub of SessionVars.GetSystemVar (same body)
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

    /// Sets a session value without validation or hooks (test setup; Go's
    /// `SetSystemVarWithoutValidation` also runs the SetSession hook, block D05.2).
    // glue: D05.2 stub (systems map only)
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
    // glue: D05.2 stub
    pub fn set_allow_mpp_execution(&mut self, on: bool) {
        self.allow_mpp_execution = on;
    }

    /// Location gets the time zone of the session.
    // glue: D05.2 stub of SessionVars.Location (timeutil.SystemLocation when unset)
    pub fn location(&self) -> Arc<Location> {
        match &self.time_zone {
            Some(l) => l.clone(),
            None => tidb_base::gostd::time::local(),
        }
    }
}
