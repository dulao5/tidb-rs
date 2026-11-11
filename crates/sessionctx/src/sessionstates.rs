//! go-file: pkg/sessionctx/sessionstates/session_states.go
//!
//! `sessionstates`: the session states carried across a session migration (`SHOW
//! SESSION_STATES` / `SET SESSION_STATES`). The file has types only. `session_token.go`
//! (signed session tokens) belongs to D04.1. The JSON encoding of `SessionStates` (tags
//! below) is done by the statement that shows the states (D12.12), not here.

use std::collections::HashMap;

use tidb_base::contextutil::SQLWarn;
use tidb_model::IndexInfo;
use tidb_types::datum::Datum;
use tidb_types::etc::FieldType;

/// PreparedStmtInfo contains the information about prepared statements, both text and
/// binary protocols.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PreparedStmtInfo {
    /// `json:"name,omitempty"`
    pub name: String,
    /// `json:"text"`
    pub stmt_text: String,
    /// `json:"db,omitempty"`
    pub stmt_db: String,
    /// `json:"types,omitempty"`
    pub param_types: Vec<u8>,
}

/// QueryInfo represents the information of last executed query. It's used to expose
/// information for test purpose.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct QueryInfo {
    /// `json:"txn_scope"`
    pub txn_scope: String,
    /// `json:"start_ts"`
    pub start_ts: u64,
    /// `json:"for_update_ts"`
    pub for_update_ts: u64,
    /// `json:"ru_consumption"`
    pub ru_consumption: f64,
    /// `json:"error,omitempty"`
    pub err_msg: String,
}

/// LastDDLInfo represents the information of last executed DDL. It's used to expose
/// information for test purpose.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LastDDLInfo {
    /// `json:"query"`
    pub query: String,
    /// `json:"seq_num"`
    pub seq_num: u64,
}

/// `HypoIndexes` of `SessionVars`: db → table → index name → index.
pub type HypoIndexes = HashMap<String, HashMap<String, HashMap<String, IndexInfo>>>;
/// `HypoTiFlashReplicas` of `SessionVars`: db → table set.
pub type HypoTiFlashReplicas = HashMap<String, std::collections::HashSet<String>>;

/// SessionStates contains all the states in the session that should be migrated when the
/// session is migrated to another server. It is shown by `show session_states` and
/// recovered by `set session_states`.
/// Go's nil maps / pointers are `None`; map iteration order is not observable here.
#[derive(Clone, Debug, Default)]
pub struct SessionStates {
    /// `json:"user-var-values,omitempty"`
    pub user_vars: Option<HashMap<String, Datum>>,
    /// `json:"user-var-types,omitempty"`
    pub user_var_types: Option<HashMap<String, FieldType>>,
    /// `json:"sys-vars,omitempty"`
    pub system_vars: Option<HashMap<String, String>>,
    /// `json:"prepared-stmts,omitempty"`
    pub prepared_stmts: Option<HashMap<u32, PreparedStmtInfo>>,
    /// `json:"prepared-stmt-id,omitempty"`
    pub prepared_stmt_id: u32,
    /// `json:"status,omitempty"`
    pub status: u32,
    /// `json:"current-db,omitempty"`
    pub current_db: String,
    /// `json:"txn-info,omitempty"`
    pub last_txn_info: String,
    /// `json:"query-info,omitempty"`
    pub last_query_info: Option<QueryInfo>,
    /// `json:"ddl-info,omitempty"`
    pub last_ddl_info: Option<LastDDLInfo>,
    /// `json:"found-rows,omitempty"`
    pub last_found_rows: u64,
    /// `json:"in-plan-cache,omitempty"`
    pub found_in_plan_cache: bool,
    /// `json:"in-binding,omitempty"`
    pub found_in_binding: bool,
    /// `json:"seq-values,omitempty"`
    pub sequence_latest_values: Option<HashMap<i64, i64>>,
    /// `json:"affected-rows,omitempty"`
    pub last_affected_rows: i64,
    /// `json:"last-insert-id,omitempty"`
    pub last_insert_id: u64,
    /// `json:"warnings,omitempty"`
    pub warnings: Vec<SQLWarn>,
    /// Define it as string to avoid cycle import. `json:"bindings,omitempty"`
    pub bindings: String,
    /// `json:"rs-group,omitempty"`
    pub resource_group_name: String,
    /// `json:"hypo-indexes,omitempty"`
    pub hypo_indexes: Option<HypoIndexes>,
    /// `json:"hypo-tiflash-replicas,omitempty"`
    pub hypo_tiflash_replicas: Option<HypoTiFlashReplicas>,
}
