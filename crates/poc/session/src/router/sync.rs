//! The session-state sync of VS1 plan §4.1: `USE`, `SET NAMES` and `SET <sysvar>` are still
//! parsed and executed by the PoC (they are D12.12, P5); after the PoC accepted one, the
//! router writes the same change into the new session, whose `SessionVars` are the source of
//! truth. Only the variables of [`SYNCED_SYSVARS`] (the ones the PoC accepts) are synced;
//! when the new session rejects a value the PoC accepted, the variable is *diverged* and step 0
//! keeps statements on the PoC until a later `SET` brings both sides together again.

use std::collections::{BTreeMap, BTreeSet};

use tidb_sessionctx::variable::session::SessionVars;
use tidb_sql::ir::{Literal, SetAssignment, SetValue};

use super::PocValues;

/// Session-scoped variables the prototype honors (or that are provably result-neutral on the
/// prototype path) and accepts via `SET`, synced into the new session.
pub const SYNCED_SYSVARS: &[&str] = &[
    // Connection / result state honored by the PoC:
    "autocommit",
    "character_set_client",
    "character_set_connection",
    "character_set_results",
    "collation_connection",
    "sql_mode",
    "sql_select_limit",
    "tidb_enable_window_function",
    // Result-neutral on the PoC path (timeouts, DML/DDL checks, and switches synced into NewSession):
    "div_precision_increment",
    "foreign_key_checks",
    "group_concat_max_len",
    "innodb_lock_wait_timeout",
    "interactive_timeout",
    "max_execution_time",
    "net_read_timeout",
    "net_write_timeout",
    "sql_safe_updates",
    "tidb_enable_noop_functions",
    "tidb_enable_prepared_plan_cache",
    "tidb_skip_isolation_level_check",
    "time_zone",
    "transaction_isolation",
    "transaction_read_only",
    "tx_isolation",
    "tx_read_only",
    "unique_checks",
    "wait_timeout",
];

/// Session-scoped variables whose semantics the prototype / read-only node cannot honor and
/// whose session `SET` stays fenced with 1235.
#[cfg(test)]
pub(crate) const FENCED_SYSVARS: &[&str] = &[
    "tidb_snapshot",
    "tidb_read_staleness",
    "tx_read_ts",
    "tidb_txn_mode",
    "tidb_replica_read",
    "tidb_read_consistency",
    "tidb_rc_read_check_ts",
];

/// Go `variable.SetNamesVariables`.
pub(crate) const SET_NAMES_VARIABLES: [&str; 3] = [
    "character_set_client",
    "character_set_connection",
    "character_set_results",
];
/// Go `variable.SetCharsetVariables`.
pub(crate) const SET_CHARSET_VARIABLES: [&str; 2] =
    ["character_set_client", "character_set_results"];

/// Removes `name` (canonicalized) and all its registered aliases from `diverged` and optional `pending_defaults`.
fn clear_aliases_in(
    diverged: &mut BTreeSet<String>,
    mut pending_defaults: Option<&mut BTreeMap<String, &'static str>>,
    name: &str,
) -> Option<&'static str> {
    if let Some(sv) = tidb_sessionctx::variable::get_sys_var(name) {
        diverged.remove(sv.name);
        if let Some(pd) = pending_defaults.as_mut() {
            pd.remove(sv.name);
        }
        for &alias in sv.aliases {
            diverged.remove(alias);
            if let Some(pd) = pending_defaults.as_mut() {
                pd.remove(alias);
            }
        }
        Some(sv.name)
    } else {
        diverged.remove(name);
        if let Some(pd) = pending_defaults {
            pd.remove(name);
        }
        None
    }
}

/// Writes `name = value` into the new session, recording whether it diverged.
pub(super) fn set(
    vars: &mut SessionVars,
    diverged: &mut BTreeSet<String>,
    name: &str,
    value: &str,
) {
    if vars.set_system_var(name, value).is_ok() {
        clear_aliases_in(diverged, None, name);
    } else {
        let canonical = tidb_sessionctx::variable::get_sys_var(name).map_or(name, |sv| sv.name);
        diverged.insert(canonical.to_owned());
    }
}

/// The text Go's `SetExecutor.getVarValue` (pkg/executor/set.go:304) makes of a session
/// `SET` value: the literal's `Datum.ToString()` (`TRUE` is the integer 1; a bare identifier is
/// its original text, `PlanBuilder.buildSet`), the empty string for `NULL`, and for `DEFAULT`
/// the global value, read through `vars`'s accessor (the cluster's `mysql.global_variables`,
/// the loader's sysvar cache, D06.5 spec §8). `None` when the prototype cannot tell (an
/// expression) or the global value cannot be read: the PoC does
/// not validate it, and the variable diverges. Used for the PoC's validation of `SET`
/// (`crate::validated_session_value`) and for the sync.
// glue: getVarValue of the literal values the prototype's SET lowers (the SET executor is D12.12)
pub(crate) fn value_text(vars: &mut SessionVars, name: &str, v: &SetValue) -> Option<String> {
    match v {
        SetValue::Literal(Literal::Int(i)) => Some(i.to_string()),
        SetValue::Literal(Literal::UInt(u)) => Some(u.to_string()),
        SetValue::Literal(Literal::Bool(b)) => Some(if *b { "1" } else { "0" }.to_owned()),
        SetValue::Literal(Literal::Str(s)) | SetValue::Ident(s) => Some(s.clone()),
        // `Datum.ToString()` of a decimal (`MyDecimal.String()`) and of a float
        // (`strconv.FormatFloat(f, 'f', -1, 64)`): `SET max_execution_time = 1.5` validates
        // the text `1.5`, which Go rejects with 1232.
        SetValue::Literal(Literal::Decimal(d)) => {
            let mut dec = tidb_types::mydecimal::MyDecimal::default();
            dec.from_string(d.as_bytes()).ok()?;
            Some(dec.string())
        }
        SetValue::Literal(Literal::Float(f)) => {
            String::from_utf8(tidb_types::datum::new_float64_datum(*f).to_string().ok()?).ok()
        }
        // `if err != nil || nativeVal.IsNull() { return "", err }`.
        SetValue::Literal(Literal::Null) => Some(String::new()),
        // `SET x = DEFAULT` of a session variable: the global value (Go
        // `SetExecutor.getVarValue` with no sysVar: `GetGlobalSystemVar`).
        SetValue::Default => vars.get_global_system_var(name).ok(),
        _ => None,
    }
}

/// After the PoC executed `SET assignments` successfully; `poc` is the PoC's state after it.
pub(super) fn sync_set(
    vars: &mut SessionVars,
    diverged: &mut BTreeSet<String>,
    pending_defaults: &mut BTreeMap<String, &'static str>,
    globals_loaded: bool,
    assignments: &[SetAssignment],
    poc: &PocValues,
) {
    for a in assignments {
        match a {
            // Go `SetExecutor.setCharset(cs, co, true)`, with the collation the PoC resolved.
            SetAssignment::Names { .. } => {
                for v in SET_NAMES_VARIABLES {
                    pending_defaults.remove(v);
                    set(vars, diverged, v, &poc.charset);
                }
                pending_defaults.remove("collation_connection");
                set(vars, diverged, "collation_connection", &poc.collation);
            }
            // Go `setCharset(cs, "", false)`: client and results, connection from the database.
            SetAssignment::Charset { .. } => {
                let cs = poc.results_charset.clone().unwrap_or_default();
                for v in SET_CHARSET_VARIABLES {
                    pending_defaults.remove(v);
                    set(vars, diverged, v, &cs);
                }
                for (var, db_var) in [
                    ("character_set_connection", "character_set_database"),
                    ("collation_connection", "collation_database"),
                ] {
                    match vars.get_global_system_var(db_var) {
                        Ok(v) => {
                            pending_defaults.remove(var);
                            set(vars, diverged, var, &v);
                        }
                        Err(_) if !globals_loaded => {
                            diverged.remove(var);
                            pending_defaults.insert(var.to_owned(), db_var);
                        }
                        Err(_) => {
                            pending_defaults.remove(var);
                            diverged.insert(var.to_owned());
                        }
                    }
                }
            }
            // Go's `setSysVariable` parses and ignores a removed variable.
            SetAssignment::SysVar {
                name,
                global: false,
                ..
            } if tidb_sessionctx::variable::removed::is_removed_sys_var(name) => {}
            SetAssignment::SysVar {
                name,
                global: false,
                ..
            } if !is_synced(name) => {
                let canonical = tidb_sessionctx::variable::get_sys_var(name)
                    .map_or(name.as_str(), |sv| sv.name);
                diverged.insert(canonical.to_owned());
            }
            SetAssignment::SysVar {
                name,
                global: false,
                value,
            } => {
                if *value == SetValue::Default && !globals_loaded {
                    if let Some(canonical) =
                        clear_aliases_in(diverged, Some(pending_defaults), name)
                    {
                        pending_defaults.insert(canonical.to_owned(), canonical);
                    }
                    continue;
                }
                let canonical =
                    clear_aliases_in(diverged, Some(pending_defaults), name).unwrap_or(name);
                match value_text(vars, canonical, value) {
                    Some(v) => set(vars, diverged, canonical, &v),
                    None => {
                        diverged.insert(canonical.to_owned());
                    }
                }
            }
            // The PoC rejects these, so they never get here.
            SetAssignment::SysVar { global: true, .. } | SetAssignment::UserVar { .. } => {}
        }
    }
}

/// After the PoC changed the current database (`USE`, `COM_INIT_DB`, the handshake).
pub(super) fn sync_db(vars: &mut SessionVars, poc: &PocValues) {
    vars.current_db = poc.current_db.clone().unwrap_or_default();
}

/// Whether `name` is synced (`SET` of anything else is rejected by the PoC).
pub fn is_synced(name: &str) -> bool {
    tidb_sessionctx::variable::get_sys_var(name).is_some_and(|sv| SYNCED_SYSVARS.contains(&sv.name))
}
