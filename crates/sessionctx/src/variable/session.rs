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
// glue: stand-in: D05.2, stmtctx.StatementContext
#[derive(Clone, Debug, Default)]
pub struct StatementContext {
    /// StmtType is the statement type (`"Set"` for SET statements).
    pub stmt_type: String,
    warnings: Vec<SQLWarn>,
}

impl StatementContext {
    /// AppendWarning appends a warning with level 'Warning'.
    // glue: stand-in: D05.2, StatementContext.AppendWarning
    pub fn append_warning(&mut self, err: Error) {
        self.warnings.push(SQLWarn {
            level: WarnLevelWarning,
            err,
        });
    }

    /// GetWarnings gets warnings.
    // glue: stand-in: D05.2, StatementContext.GetWarnings
    pub fn get_warnings(&self) -> Vec<SQLWarn> {
        self.warnings.clone()
    }

    /// SetWarnings sets warnings.
    // glue: stand-in: D05.2, StatementContext.SetWarnings
    pub fn set_warnings(&mut self, warns: Vec<SQLWarn>) {
        self.warnings = warns;
    }
}

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
}

/// The fields of `SessionVars` the D05.1 validation hooks use.
// glue: stand-in: D05.2, SessionVars
pub struct SessionVars {
    /// StmtCtx holds variables for current executing statement.
    pub stmt_ctx: StatementContext,
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
    /// A session with Go's `NewSessionVars` defaults for the fields above.
    // glue: stand-in: D05.2, NewSessionVars
    pub fn new(global_vars_accessor: Box<dyn GlobalVarAccessor>) -> SessionVars {
        SessionVars {
            stmt_ctx: StatementContext::default(),
            global_vars_accessor,
            systems: HashMap::from([
                // charset.GetDefaultCharsetAndCollate()
                (CharacterSetConnection.to_owned(), "utf8mb4".to_owned()),
                (CollationConnection.to_owned(), "utf8mb4_bin".to_owned()),
            ]),
            fields: super::session_fields::SessionVarsFields::default(),
            time_zone: None,
            tls_connection_state: None,
        }
    }

    /// GetSessionOrGlobalSystemVar gets a system variable.
    /// If it is a session only variable, use the default value defined in code.
    /// Returns error if there is no such variable.
    // go: SessionVars.GetSessionOrGlobalSystemVar L1 (no context)
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
    // go: SessionVars.GetGlobalSystemVar L1 (no context)
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
    ) -> Result<String, Error> {
        let Some(sv) = get_sys_var(name) else {
            return Err(
                ErrUnknownSystemVar.gen_with_stack_by_args(vec![Arg::from(name.to_owned())])
            );
        };
        let (val, err) = sv.validate(self, val, ScopeSession);
        err?;
        // The map s.systems[sv.Name] is lazy initialized. If we directly read it, we might read empty result.
        // Since this code path is not a hot path, we directly call GetSessionOrGlobalSystemVar to get the value safely.
        let old_v = self.get_session_or_global_system_var(sv.name)?;
        sv.set_session_from_hook(self, &val)?;
        Ok(old_v)
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
    // glue: stand-in: D05.2, SessionVars.GetSystemVar (same body)
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

    /// Location gets the time zone of the session.
    // glue: stand-in: D05.2, SessionVars.Location (timeutil.SystemLocation when unset)
    pub fn location(&self) -> Arc<Location> {
        match &self.time_zone {
            Some(l) => l.clone(),
            None => tidb_base::gostd::time::local(),
        }
    }
}
