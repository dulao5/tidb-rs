//! The session-state sync of VS1 plan §4.1: `USE`, `SET NAMES` and `SET <sysvar>` are still
//! parsed and executed by the PoC (they are D12.12, P5); after the PoC accepted one, the
//! router writes the same change into the new session, whose `SessionVars` are the source of
//! truth. Only the variables of [`SYNCED_SYSVARS`] (the ones the PoC accepts) are synced.
//! When the new session rejects a value the PoC accepted, the variable is *diverged*: the
//! new session does not know it, so step 0 of the routing order sends statements to the PoC
//! until a later `SET` brings both sides together again.

use std::collections::BTreeSet;

use tidb_sessionctx::variable::session::SessionVars;
use tidb_sql::ir::{Literal, SetAssignment, SetValue};

use super::PocValues;

/// The system variables the PoC accepts (`Session::set`), synced into the new session. The
/// PoC models some of them (charsets) and accepts the rest without effect; the new session
/// applies all of them with Go's semantics.
pub const SYNCED_SYSVARS: &[&str] = &[
    "autocommit",
    "character_set_client",
    "character_set_connection",
    "character_set_results",
    "collation_connection",
    "sql_mode",
    "wait_timeout",
    "interactive_timeout",
    "net_write_timeout",
    "net_read_timeout",
    "transaction_isolation",
    "tx_isolation",
    "transaction_read_only",
    "tx_read_only",
    "time_zone",
    "max_execution_time",
];

/// Go `variable.SetNamesVariables`.
const SET_NAMES_VARIABLES: [&str; 3] = [
    "character_set_client",
    "character_set_connection",
    "character_set_results",
];
/// Go `variable.SetCharsetVariables`.
const SET_CHARSET_VARIABLES: [&str; 2] = ["character_set_client", "character_set_results"];

/// Writes `name = value` into the new session, recording whether it diverged.
fn set(vars: &mut SessionVars, diverged: &mut BTreeSet<String>, name: &str, value: &str) {
    if vars.set_system_var(name, value).is_ok() {
        diverged.remove(name);
    } else {
        diverged.insert(name.to_owned());
    }
}

/// The text Go's `SET` would store for a literal value; `None` when the router cannot tell
/// (an expression): the variable diverges.
fn value_text(name: &str, v: &SetValue) -> Option<String> {
    match v {
        SetValue::Literal(Literal::Int(i)) => Some(i.to_string()),
        SetValue::Literal(Literal::UInt(u)) => Some(u.to_string()),
        SetValue::Literal(Literal::Bool(b)) => Some(if *b { "1" } else { "0" }.to_owned()),
        SetValue::Literal(Literal::Str(s)) | SetValue::Ident(s) => Some(s.clone()),
        // `SET character_set_results = NULL`: Go stores the empty string.
        SetValue::Literal(Literal::Null) if name == "character_set_results" => Some(String::new()),
        // `SET x = DEFAULT`: the global value (here the compiled default).
        SetValue::Default => {
            tidb_sessionctx::variable::get_sys_var(name).map(|sv| sv.value.clone())
        }
        _ => None,
    }
}

/// After the PoC executed `SET assignments` successfully; `poc` is the PoC's state after it.
pub(super) fn sync_set(
    vars: &mut SessionVars,
    diverged: &mut BTreeSet<String>,
    assignments: &[SetAssignment],
    poc: &PocValues,
) {
    for a in assignments {
        match a {
            // Go `SetExecutor.setCharset(cs, co, true)`, with the collation the PoC resolved.
            SetAssignment::Names { .. } => {
                for v in SET_NAMES_VARIABLES {
                    set(vars, diverged, v, &poc.charset);
                }
                set(vars, diverged, "collation_connection", &poc.collation);
            }
            // Go `setCharset(cs, "", false)`: client and results, connection from the database.
            SetAssignment::Charset { .. } => {
                let cs = poc.results_charset.clone().unwrap_or_default();
                for v in SET_CHARSET_VARIABLES {
                    set(vars, diverged, v, &cs);
                }
                for (var, db_var) in [
                    ("character_set_connection", "character_set_database"),
                    ("collation_connection", "collation_database"),
                ] {
                    match vars.get_global_system_var(db_var) {
                        Ok(v) => set(vars, diverged, var, &v),
                        Err(_) => {
                            diverged.insert(var.to_owned());
                        }
                    }
                }
            }
            SetAssignment::SysVar {
                name,
                global: false,
                ..
            } if !is_synced(name) => {
                diverged.insert(name.clone());
            }
            SetAssignment::SysVar {
                name,
                global: false,
                value,
            } => match value_text(name, value) {
                Some(v) => set(vars, diverged, name, &v),
                None => {
                    diverged.insert(name.clone());
                }
            },
            // The PoC rejects these, so they never get here.
            SetAssignment::SysVar { global: true, .. } | SetAssignment::UserVar { .. } => {}
        }
    }
}

/// After the PoC changed the current database (`USE`, `COM_INIT_DB`, the handshake).
pub(super) fn sync_db(vars: &mut SessionVars, poc: &PocValues) {
    vars.current_db = poc.current_db.clone().unwrap_or_default();
}

/// Whether `name` is synced (a `SET` of anything else is rejected by the PoC).
pub fn is_synced(name: &str) -> bool {
    SYNCED_SYSVARS.contains(&name)
}
