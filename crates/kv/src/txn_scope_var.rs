//! go-file: pkg/kv/txn_scope_var.go

// Go identifiers for the constants (M-GONAME).
#![allow(non_upper_case_globals)]

use tidb_base::config;

/// TxnScopeVar indicates the used txnScope for oracle
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TxnScopeVar {
    /// varValue indicates the value of @@txn_scope, which can only be `global` or `local`
    var_value: String,
    /// txnScope indicates the value which the tidb-server holds to request tso to pd
    txn_scope: String,
}

/// NewDefaultTxnScopeVar creates a default TxnScopeVar according to the config.
/// If zone label is set, we will check whether it's not the GlobalTxnScope and create a new
/// Local TxnScopeVar. If zone label is not set, we will create a new Global TxnScopeVar.
// go: NewDefaultTxnScopeVar L1
pub fn new_default_txn_scope_var() -> TxnScopeVar {
    let txn_scope = config::get_txn_scope_from_config();
    if txn_scope != GlobalTxnScope {
        return new_local_txn_scope_var(&txn_scope);
    }
    new_global_txn_scope_var()
}

/// NewGlobalTxnScopeVar creates a Global TxnScopeVar
// go: NewGlobalTxnScopeVar L1
pub fn new_global_txn_scope_var() -> TxnScopeVar {
    new_txn_scope_var(GlobalTxnScope, GlobalTxnScope)
}

/// NewLocalTxnScopeVar creates a Local TxnScopeVar with given real txnScope value.
// go: NewLocalTxnScopeVar L1
pub fn new_local_txn_scope_var(txn_scope: &str) -> TxnScopeVar {
    new_txn_scope_var(LocalTxnScope, txn_scope)
}

impl TxnScopeVar {
    /// GetVarValue returns the value of @@txn_scope which can only be `global` or `local`
    // go: TxnScopeVar.GetVarValue L1
    pub fn get_var_value(&self) -> &str {
        &self.var_value
    }

    /// GetTxnScope returns the value of the tidb-server holds to request tso to pd.
    /// When varValue is 'global`, directly return global here
    // go: TxnScopeVar.GetTxnScope L1
    pub fn get_txn_scope(&self) -> &str {
        &self.txn_scope
    }
}

// go: newTxnScopeVar L1
fn new_txn_scope_var(var_value: &str, txn_scope: &str) -> TxnScopeVar {
    TxnScopeVar {
        var_value: var_value.to_owned(),
        txn_scope: txn_scope.to_owned(),
    }
}

/// GlobalTxnScope is synced with PD's define of global scope.
/// If we want to remove the dependency on store/tikv here, we need to map
/// the two GlobalTxnScopes in the driver layer.
pub const GlobalTxnScope: &str = "global";
/// LocalTxnScope indicates the transaction should use local ts.
pub const LocalTxnScope: &str = "local";
