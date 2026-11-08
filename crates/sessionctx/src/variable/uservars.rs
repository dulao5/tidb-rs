//! `variable.UserVarsReader` (Go `pkg/sessionctx/variable/session.go`, block D05.2): the user
//! variables an expression reads. D05.2 has not ported user variables yet; the interface and
//! an empty `UserVars` live here (where Go defines them) as a stand-in until D05.2 replaces
//! them. `exprctx` re-exports them.

use std::sync::Arc;

use tidb_types::datum::Datum;

/// UserVarsReader is used to read user defined variables.
// glue: stand-in: D05.2, variable.UserVarsReader (interface)
pub trait UserVarsReader: Send + Sync {
    /// GetUserVarVal get user defined variables' value
    // glue: stand-in: D05.2, interface method UserVarsReader.GetUserVarVal
    fn get_user_var_val(&self, name: &str) -> Option<Datum>;
    /// Clone clones the user vars
    // glue: stand-in: D05.2, interface method UserVarsReader.Clone
    fn clone_reader(&self) -> UserVarsReaderRef;
}

/// A shared `variable.UserVarsReader` interface value.
pub type UserVarsReaderRef = Arc<dyn UserVarsReader>;

/// `variable.UserVars` with no variable set (what `NewUserVars()` returns).
// glue: stand-in: D05.2, variable.UserVars
#[derive(Debug, Default)]
pub struct EmptyUserVars;

impl UserVarsReader for EmptyUserVars {
    // glue: stand-in: D05.2, UserVars.GetUserVarVal on an empty map
    fn get_user_var_val(&self, _name: &str) -> Option<Datum> {
        None
    }

    // glue: stand-in: D05.2, UserVars.Clone of an empty map
    fn clone_reader(&self) -> UserVarsReaderRef {
        Arc::new(EmptyUserVars)
    }
}

/// `variable.NewUserVars()`.
// glue: stand-in: D05.2, variable.NewUserVars
pub fn new_user_vars() -> UserVarsReaderRef {
    Arc::new(EmptyUserVars)
}
