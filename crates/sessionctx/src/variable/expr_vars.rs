//! The narrow view of `SessionVars` that expression evaluation needs (D05.2 PR E; Lucky's
//! review of tidb-rs #71).
//!
//! Go's `expression` package reaches the session through `EvalContext` and, in 45 places,
//! reads or writes `SessionVars` directly: the user, the previous statement's last insert
//! id and affected rows, `CurrInsertValues`, system variables; and it WRITES the last insert
//! id (`LAST_INSERT_ID(expr)`) and user variables (`@a := ...`). Evaluation runs on executor
//! workers, which never get `&mut SessionVars` (N2 §3), so the view is `&self` only and every
//! written field is internally synchronized: the last insert id is atomic in the statement
//! context, user variables are an `Arc<UserVars>` with their own lock.
//!
//! `SessionVars` implements it for the session thread. The executor-side implementation
//! (`Send + Sync`, built when a statement starts) comes with the session main flow (D05.3).
//! Not yet in the view, because their types are not ported: the user identity
//! (`auth.UserIdentity`, D04.1) and `CurrInsertValues` (`chunk.Row` of the INSERT being
//! executed; the write path is X, kept for `VALUES()` in `ON DUPLICATE KEY UPDATE`).
//! `tidb-expression` will take `&dyn ExprSessionVars` (defined here so the expression crate,
//! one rank above, does not depend on `SessionVars`).

use std::sync::Arc;

use super::session::SessionVars;
use super::uservars::UserVars;

/// The `SessionVars` reads and writes of expression evaluation.
// glue: N2 narrow interface over SessionVars (no Go counterpart)
pub trait ExprSessionVars {
    /// `vars.StmtCtx.PrevLastInsertID` (`LAST_INSERT_ID()`).
    fn prev_last_insert_id(&self) -> u64;
    /// `vars.StmtCtx.PrevAffectedRows` (`ROW_COUNT()`).
    fn prev_affected_rows(&self) -> i64;
    /// `vars.SetLastInsertID` (`LAST_INSERT_ID(expr)`).
    fn set_last_insert_id(&self, insert_id: u64);
    /// `vars.GetUserVarVal` / `SetUserVarVal` / `GetUserVarType` go through the shared table.
    fn user_vars(&self) -> Arc<UserVars>;
    /// `vars.GetSystemVar` (`@@name` of session-only variables read at evaluation time).
    fn get_system_var(&self, name: &str) -> (String, bool);
}

impl ExprSessionVars for SessionVars {
    // glue: N2 narrow interface
    fn prev_last_insert_id(&self) -> u64 {
        self.stmt_ctx().prev_last_insert_id
    }

    // glue: N2 narrow interface
    fn prev_affected_rows(&self) -> i64 {
        self.stmt_ctx().prev_affected_rows
    }

    // glue: N2 narrow interface
    fn set_last_insert_id(&self, insert_id: u64) {
        SessionVars::set_last_insert_id(self, insert_id);
    }

    // glue: N2 narrow interface
    fn user_vars(&self) -> Arc<UserVars> {
        Arc::clone(&self.state.user_vars)
    }

    // glue: N2 narrow interface
    fn get_system_var(&self, name: &str) -> (String, bool) {
        SessionVars::get_system_var(self, name)
    }
}
