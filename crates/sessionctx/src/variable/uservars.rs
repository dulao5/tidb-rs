//! go-file: pkg/sessionctx/variable/session.go
//!
//! User-defined variables (`@a`): `variable.UserVarsReader` and `variable.UserVars` (block
//! D05.2). `exprctx` re-exports them.
//!
//! Go's `UserVars` guards two maps with a `sync.RWMutex` and is shared by pointer (the
//! session and the expression contexts that read it); here it is shared by `Arc` with a
//! `parking_lot::RwLock` around both maps. `GetUserVarType` returns a clone of the stored
//! `FieldType` (Go returns the stored pointer; callers in TiDB only read it).

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;
use tidb_types::datum::Datum;
use tidb_types::etc::FieldType;

/// UserVarsReader is used to read user defined variables.
// glue: Go interface variable.UserVarsReader
pub trait UserVarsReader: Send + Sync {
    /// GetUserVarVal get user defined variables' value
    // glue: interface method UserVarsReader.GetUserVarVal
    fn get_user_var_val(&self, name: &str) -> Option<Datum>;
    /// GetUserVarType get user defined variables' type
    // glue: interface method UserVarsReader.GetUserVarType
    fn get_user_var_type(&self, name: &str) -> Option<FieldType>;
    /// Clone clones the user vars
    // glue: interface method UserVarsReader.Clone
    fn clone_reader(&self) -> UserVarsReaderRef;
}

/// A shared `variable.UserVarsReader` interface value.
pub type UserVarsReaderRef = Arc<dyn UserVarsReader>;

/// UserVars is used to provide user variable operations.
#[derive(Debug, Default)]
pub struct UserVars {
    /// lock is for user defined variables. values and types is read/write protected.
    inner: RwLock<UserVarsInner>,
}

// glue: the two maps Go guards with UserVars.lock
#[derive(Debug, Default)]
struct UserVarsInner {
    /// values stores the Datum for user variables
    values: HashMap<String, Datum>,
    /// types stores the FieldType for user variables, it cannot be inferred from values when values have not been set yet.
    types: HashMap<String, FieldType>,
}

/// NewUserVars creates a new user UserVars object
// go: NewUserVars L1
pub fn new_user_vars() -> Arc<UserVars> {
    Arc::new(UserVars::default())
}

impl UserVars {
    /// SetUserVarVal set user defined variables' value
    // go: UserVars.SetUserVarVal L1
    pub fn set_user_var_val(&self, name: &str, dt: Datum) {
        self.inner.write().values.insert(name.to_owned(), dt);
    }

    /// UnsetUserVar unset an user defined variable by name.
    // go: UserVars.UnsetUserVar L1
    pub fn unset_user_var(&self, var_name: &str) {
        let var_name = tidb_base::gostd::to_lower(var_name);
        let mut inner = self.inner.write();
        inner.values.remove(&var_name);
        inner.types.remove(&var_name);
    }

    /// SetUserVarType set user defined variables' type
    // go: UserVars.SetUserVarType L1
    pub fn set_user_var_type(&self, name: &str, ft: FieldType) {
        self.inner.write().types.insert(name.to_owned(), ft);
    }
}

impl UserVarsReader for UserVars {
    /// GetUserVarVal get user defined variables' value
    // go: UserVars.GetUserVarVal L1
    fn get_user_var_val(&self, name: &str) -> Option<Datum> {
        self.inner.read().values.get(name).cloned()
    }

    /// GetUserVarType get user defined variables' type
    // go: UserVars.GetUserVarType L2 (returns a clone of the stored type)
    fn get_user_var_type(&self, name: &str) -> Option<FieldType> {
        self.inner.read().types.get(name).cloned()
    }

    /// Clone clones the user vars
    // go: UserVars.Clone L1
    fn clone_reader(&self) -> UserVarsReaderRef {
        let inner = self.inner.write();
        Arc::new(UserVars {
            inner: RwLock::new(UserVarsInner {
                values: inner.values.clone(),
                types: inner.types.clone(),
            }),
        })
    }
}
