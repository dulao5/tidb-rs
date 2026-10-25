//! SQL sessions of the read-only node: implements the protocol layer's [`SessionFactory`] /
//! [`SessionHandler`] on top of the planner (`tidb-sql`) and executors (`tidb-executor`).
//!
//! Every SELECT runs as its own autocommit read: a fresh TSO `start_ts`, the schema visible
//! at that timestamp (`SchemaCache::info_schema_at`), and a KV snapshot at it. Prepared
//! statements keep the lowered statement and are re-planned on every execute.

#![forbid(unsafe_code)]

pub mod constant;

use std::collections::HashMap;
use std::sync::Arc;

use tidb_distsql::DagContext;
use tidb_executor::ExecContext;
use tidb_kv::KvClient;
use tidb_meta::InfoSchema;
use tidb_meta::SchemaCache;
use tidb_mysql_protocol::ConnectionInfo;
use tidb_mysql_protocol::Credential;
use tidb_mysql_protocol::OkInfo;
use tidb_mysql_protocol::Param;
use tidb_mysql_protocol::PreparedInfo;
use tidb_mysql_protocol::Response;
use tidb_mysql_protocol::ResultSet;
use tidb_mysql_protocol::SessionFactory;
use tidb_mysql_protocol::SessionHandler;
use tidb_mysql_protocol::SqlError;
use tidb_mysql_protocol::Value;
use tidb_mysql_protocol::constants::collation;
use tidb_sql::Lowered;
use tidb_sql::PlanContext;
use tidb_sql::Statement;
use tidb_sql::ir::Literal;
use tidb_sql::ir::Select;
use tidb_sql::ir::SetAssignment;
use tidb_sql::ir::SetValue;
use tidb_sql::ranges::param_literal;
use tidb_sql::types::apply_result_charset;

use crate::constant::ConstEnv;

/// State shared by all sessions.
pub struct Shared {
    pub kv: KvClient,
    pub schema: SchemaCache,
    pub dag: DagContext,
    /// The single account accepted until users are read from the cluster.
    pub user: String,
    pub credential: Credential,
    pub server_version: String,
    pub tls_enabled: bool,
}

/// Creates [`Session`]s.
#[derive(Clone)]
pub struct Factory {
    shared: Arc<Shared>,
}

impl Factory {
    pub fn new(shared: Shared) -> Factory {
        Factory {
            shared: Arc::new(shared),
        }
    }
}

impl SessionFactory for Factory {
    type Session = Session;

    async fn lookup_user(&self, user: &str, _host: &str) -> Option<Credential> {
        (user == self.shared.user).then(|| self.shared.credential.clone())
    }

    fn open_session(&self, info: &ConnectionInfo) -> Result<Session, SqlError> {
        let host = info
            .peer
            .rsplit_once(':')
            .map_or(info.peer.as_str(), |(h, _)| h);
        Ok(Session {
            shared: self.shared.clone(),
            current_db: None,
            user: format!(
                "{}@{}",
                info.user,
                host.trim_matches(|c| c == '[' || c == ']')
            ),
            current_user: format!("{}@%", info.user),
            connection_id: info.connection_id,
            charset: "utf8mb4".to_owned(),
            collation: "utf8mb4_bin".to_owned(),
            results_charset: Some("utf8mb4".to_owned()),
            stmts: HashMap::new(),
        })
    }
}

/// One client connection's session.
pub struct Session {
    shared: Arc<Shared>,
    /// Current database as given to `USE` (Go `CurrentDB`).
    current_db: Option<String>,
    user: String,
    current_user: String,
    connection_id: u64,
    /// `SET NAMES` charset / collation (`character_set_client`/`connection`).
    charset: String,
    collation: String,
    /// `@@character_set_results` (`None` = NULL: columns keep their own charset).
    results_charset: Option<String>,
    /// Prepared statements by id.
    stmts: HashMap<u32, Arc<Lowered>>,
}

fn kv_error(e: impl std::fmt::Display) -> SqlError {
    SqlError::unknown(e.to_string())
}

fn ok() -> Response {
    Response::Ok(OkInfo::default())
}

fn not_in_txn(what: &str) -> SqlError {
    SqlError::not_supported(format!(
        "{what}: explicit transactions are not supported by the read-only node (every \
         statement runs in autocommit mode)"
    ))
}

/// Whether `SET var = value` turns autocommit on (`Some(true)`), off, or is not a boolean.
fn bool_value(v: &SetValue) -> Option<bool> {
    match v {
        SetValue::Literal(Literal::Int(i)) => Some(*i != 0),
        SetValue::Literal(Literal::UInt(u)) => Some(*u != 0),
        SetValue::Literal(Literal::Bool(b)) => Some(*b),
        SetValue::Literal(Literal::Str(s)) | SetValue::Ident(s) => {
            match s.to_ascii_lowercase().as_str() {
                "on" | "1" | "true" => Some(true),
                "off" | "0" | "false" => Some(false),
                _ => None,
            }
        }
        SetValue::Default => Some(true),
        _ => None,
    }
}

fn set_string(v: &SetValue) -> Option<Option<String>> {
    match v {
        SetValue::Literal(Literal::Str(s)) | SetValue::Ident(s) => Some(Some(s.to_lowercase())),
        SetValue::Literal(Literal::Null) => Some(None),
        _ => None,
    }
}

/// Session variables accepted and ignored: they do not change the results of anything this
/// node runs.
const IGNORED_SYSVARS: &[&str] = &[
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

impl Session {
    fn const_env(&self) -> ConstEnv<'_> {
        ConstEnv {
            server_version: &self.shared.server_version,
            current_db: self.current_db.as_deref(),
            user: &self.user,
            current_user: &self.current_user,
            connection_id: self.connection_id,
            have_ssl: if self.shared.tls_enabled {
                "YES"
            } else {
                "DISABLED"
            },
            autocommit: true,
            charset: &self.charset,
            collation: &self.collation,
        }
    }

    /// A start timestamp and the schema visible at it.
    async fn begin_read(&self) -> Result<(u64, Arc<InfoSchema>), SqlError> {
        let ts = self.shared.kv.current_ts().await.map_err(kv_error)?;
        let is = self
            .shared
            .schema
            .info_schema_at(ts)
            .await
            .map_err(kv_error)?;
        Ok((ts, is))
    }

    async fn run(&mut self, lowered: &Lowered, params: &[Literal]) -> Result<Response, SqlError> {
        match &lowered.stmt {
            Statement::Select(sel) => self.select(sel, params).await.map(Response::ResultSet),
            Statement::Use(db) => {
                self.use_db(db).await?;
                Ok(ok())
            }
            Statement::Set(assignments) => {
                self.set(assignments)?;
                Ok(ok())
            }
            Statement::Begin => Err(not_in_txn("BEGIN/START TRANSACTION")),
            Statement::Commit => Err(not_in_txn("COMMIT")),
            Statement::Rollback => Err(not_in_txn("ROLLBACK")),
            Statement::ShowWarnings { .. } => Ok(Response::ResultSet(show_warnings())),
        }
    }

    async fn select(&self, sel: &Select, params: &[Literal]) -> Result<ResultSet, SqlError> {
        if sel.from.is_none() {
            return self.const_env().select(sel, params);
        }
        let (ts, is) = self.begin_read().await?;
        let plan = tidb_sql::plan_select(
            sel,
            &is,
            PlanContext {
                current_db: self.current_db.as_deref(),
                params,
                describe_only: false,
            },
        )?;
        let mut snapshot = self.shared.kv.snapshot(ts);
        let cx = ExecContext {
            dag: self.shared.dag.clone(),
            schema_version: is.version(),
        };
        let rows = tidb_executor::execute(&plan, &mut snapshot, &cx).await?;
        let mut columns = plan.columns;
        for c in &mut columns {
            apply_result_charset(c, self.results_charset.as_deref());
        }
        Ok(ResultSet {
            columns,
            rows,
            warnings: 0,
        })
    }

    fn set(&mut self, assignments: &[SetAssignment]) -> Result<(), SqlError> {
        // Validate everything first, then apply (a failing SET changes nothing).
        let mut names = None;
        let mut results = None;
        for a in assignments {
            match a {
                SetAssignment::Names { charset, collation } => {
                    let cs = charset.clone().unwrap_or_else(|| "utf8mb4".to_owned());
                    let cs = cs.to_lowercase();
                    if !matches!(
                        cs.as_str(),
                        "utf8mb4" | "utf8" | "binary" | "latin1" | "ascii"
                    ) {
                        return Err(SqlError::not_supported(format!("SET NAMES {cs}")));
                    }
                    let coll = collation
                        .clone()
                        .map(|c| c.to_lowercase())
                        .unwrap_or_else(|| {
                            match cs.as_str() {
                                "utf8mb4" => "utf8mb4_bin",
                                "utf8" => "utf8_bin",
                                "latin1" => "latin1_bin",
                                "ascii" => "ascii_bin",
                                _ => "binary",
                            }
                            .to_owned()
                        });
                    names = Some((cs.clone(), coll));
                    results = Some(Some(cs));
                }
                SetAssignment::Charset { charset } => {
                    let cs = charset
                        .clone()
                        .unwrap_or_else(|| "utf8mb4".to_owned())
                        .to_lowercase();
                    if !matches!(
                        cs.as_str(),
                        "utf8mb4" | "utf8" | "binary" | "latin1" | "ascii"
                    ) {
                        return Err(SqlError::not_supported(format!("SET CHARACTER SET {cs}")));
                    }
                    results = Some(Some(cs));
                }
                SetAssignment::UserVar { .. } => {
                    return Err(SqlError::not_supported("user variables"));
                }
                SetAssignment::SysVar {
                    name,
                    global,
                    value,
                } => {
                    if *global {
                        return Err(SqlError::not_supported("SET GLOBAL"));
                    }
                    match name.as_str() {
                        "autocommit" => match bool_value(value) {
                            Some(true) => {}
                            Some(false) => return Err(not_in_txn("autocommit=0")),
                            None => {
                                return Err(SqlError::new(
                                    1231,
                                    "Variable 'autocommit' can't be set to the value given",
                                ));
                            }
                        },
                        "character_set_results" => match set_string(value) {
                            Some(v) => results = Some(v),
                            None => return Err(SqlError::not_supported("this SET value")),
                        },
                        "character_set_client"
                        | "character_set_connection"
                        | "collation_connection" => {
                            if set_string(value).is_none() && *value != SetValue::Default {
                                return Err(SqlError::not_supported("this SET value"));
                            }
                        }
                        "sql_mode" => match value {
                            SetValue::Literal(Literal::Str(s))
                                if !s.to_ascii_uppercase().contains("PAD_CHAR_TO_FULL_LENGTH") => {}
                            SetValue::Default => {}
                            _ => {
                                return Err(SqlError::not_supported(
                                    "this sql_mode (PAD_CHAR_TO_FULL_LENGTH changes CHAR results)",
                                ));
                            }
                        },
                        n if IGNORED_SYSVARS.contains(&n) => {}
                        n => {
                            return Err(SqlError::not_supported(format!(
                                "setting system variable '{n}'"
                            )));
                        }
                    }
                }
            }
        }
        if let Some((cs, coll)) = names {
            self.charset = cs;
            self.collation = coll;
        }
        if let Some(r) = results {
            self.results_charset = r;
        }
        Ok(())
    }
}

/// `SHOW WARNINGS`: statements here never leave warnings.
fn show_warnings() -> ResultSet {
    use tidb_mysql_protocol::ColumnDef;
    use tidb_mysql_protocol::ColumnType;
    let mut level = ColumnDef::new("Level", ColumnType::VarString).with_length(28);
    let mut code = ColumnDef::new("Code", ColumnType::Long).with_length(4);
    let mut message = ColumnDef::new("Message", ColumnType::VarString).with_length(2048);
    for c in [&mut level, &mut code, &mut message] {
        c.org_name.clear();
    }
    level.charset = collation::UTF8MB4_BIN;
    message.charset = collation::UTF8MB4_BIN;
    ResultSet {
        columns: vec![level, code, message],
        rows: Vec::new(),
        warnings: 0,
    }
}

fn params_to_literals(params: Vec<Param>) -> Result<Vec<Literal>, SqlError> {
    params
        .into_iter()
        .map(|p| match (p.unsigned, p.value) {
            (true, Value::Int(i)) => Ok(Literal::UInt(i as u64)),
            (_, v) => param_literal(&v),
        })
        .collect()
}

impl SessionHandler for Session {
    async fn query(&mut self, sql: &str) -> Result<Response, SqlError> {
        let lowered = tidb_sql::parse_and_lower(sql)?;
        if lowered.param_count > 0 {
            return Err(SqlError::new(
                1064,
                "You have an error in your SQL syntax; '?' is only allowed in prepared statements",
            ));
        }
        self.run(&lowered, &[]).await
    }

    async fn prepare(&mut self, stmt_id: u32, sql: &str) -> Result<PreparedInfo, SqlError> {
        let lowered = tidb_sql::parse_and_lower(sql)?;
        let param_count = u16::try_from(lowered.param_count).map_err(|_| {
            SqlError::new(1390, "Prepared statement contains too many placeholders")
        })?;
        let mut columns = Vec::new();
        if let Statement::Select(sel) = &lowered.stmt {
            if sel.from.is_some() {
                // Resolve names now, like Go's prepare: unknown tables/columns fail here.
                let (_, is) = self.begin_read().await?;
                let plan = tidb_sql::plan_select(
                    sel,
                    &is,
                    PlanContext {
                        current_db: self.current_db.as_deref(),
                        params: &[],
                        describe_only: true,
                    },
                )?;
                columns = plan.columns;
                for c in &mut columns {
                    apply_result_charset(c, self.results_charset.as_deref());
                }
            } else if param_count == 0 {
                columns = self.const_env().select(sel, &[])?.columns;
            }
        }
        self.stmts.insert(stmt_id, Arc::new(lowered));
        Ok(PreparedInfo {
            param_count,
            columns,
        })
    }

    async fn execute(&mut self, stmt_id: u32, params: Vec<Param>) -> Result<Response, SqlError> {
        let lowered = self
            .stmts
            .get(&stmt_id)
            .cloned()
            .ok_or_else(|| SqlError::unknown_stmt(stmt_id, "stmt_execute"))?;
        let params = params_to_literals(params)?;
        self.run(&lowered, &params).await
    }

    fn close(&mut self, stmt_id: u32) {
        self.stmts.remove(&stmt_id);
    }

    async fn use_db(&mut self, db: &str) -> Result<(), SqlError> {
        let (_, is) = self.begin_read().await?;
        if is.schema_by_name(db).is_none() {
            return Err(SqlError::bad_db(db));
        }
        self.current_db = Some(db.to_owned());
        Ok(())
    }
}

/// Reads `new_collation_enabled` from `mysql.tidb` (set once at cluster bootstrap; Go:
/// `collate.SetNewCollationEnabledForTest` from `bootstrap`). Missing row: old framework.
pub async fn detect_new_collation(kv: &KvClient, schema: &SchemaCache) -> Result<bool, SqlError> {
    let ts = kv.current_ts().await.map_err(kv_error)?;
    let is = schema.info_schema_at(ts).await.map_err(kv_error)?;
    let lowered =
        tidb_sql::parse_and_lower("SELECT VARIABLE_NAME, VARIABLE_VALUE FROM mysql.tidb")?;
    let Statement::Select(sel) = &lowered.stmt else {
        unreachable!("a SELECT")
    };
    let plan = tidb_sql::plan_select(
        sel,
        &is,
        PlanContext {
            current_db: None,
            params: &[],
            describe_only: false,
        },
    )?;
    let mut snapshot = kv.snapshot(ts);
    let cx = ExecContext {
        dag: DagContext::select_utc(),
        schema_version: is.version(),
    };
    let rows = tidb_executor::execute(&plan, &mut snapshot, &cx).await?;
    Ok(rows.iter().any(|r| {
        r.first() == Some(&Value::from("new_collation_enabled"))
            && matches!(r.get(1), Some(Value::Bytes(b)) if b.eq_ignore_ascii_case(b"true"))
    }))
}
