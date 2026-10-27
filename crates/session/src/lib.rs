//! SQL sessions of the read-only node: implements the protocol layer's [`SessionFactory`] /
//! [`SessionHandler`] on top of the planner (`tidb-sql`) and executors (`tidb-executor`).
//!
//! **Synced mode** (`--schema-sync`, ADR 0005): while the node is registered and in sync with
//! the cluster (`tidb_domain::Registry::admit`), a SELECT uses the newest schema loaded in the
//! background and holds a metadata-lock entry until its response is written. Autocommit point
//! gets by handle read at MaxTS without a TSO; other reads take a TSO but no schema check. If
//! the node left sync before the read finished, the result is discarded and the statement
//! runs on the path below.
//!
//! **Fallback / `--schema-sync=off`** (ADR 0003): every SELECT runs as its own autocommit
//! read: a fresh TSO `start_ts`, the schema visible
//! at that timestamp, and a KV snapshot at it. The TSO request is sent as soon as the
//! statement is known to read a table and awaited only when the read needs it. The statement
//! is planned with the newest cached schema and the schema-version check at `start_ts` runs
//! concurrently with the data read; a result is used only if the check proves that schema is
//! the one visible at `start_ts`, otherwise the statement is re-planned and re-read with it
//! (ADR 0003, "Optimistic schema check"). Prepared statements keep the lowered statement and
//! are re-planned on every execute.

#![forbid(unsafe_code)]

pub mod constant;
pub mod timing;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use tidb_distsql::DagContext;
use tidb_domain::Admission;
use tidb_domain::SchemaSync;
use tidb_domain::StmtGuard;
use tidb_executor::ExecContext;
use tidb_kv::KvClient;
use tidb_meta::InfoSchema;
use tidb_meta::SchemaCache;
use tidb_meta::SchemaCheck;
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
use crate::timing::Kind;
use crate::timing::Phase;
use crate::timing::Timer;

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
    /// Schema sync (ADR 0005); `None` with `--schema-sync=off`.
    pub sync: Option<SchemaSync>,
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

    /// The state shared by this factory's sessions.
    pub fn shared(&self) -> &Shared {
        &self.shared
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
        // Go: the handshake collation sets character_set_client/connection/results and
        // collation_connection; unknown ids fall back to the server default.
        let (charset, collation) = match info.collation {
            45 => ("utf8mb4", "utf8mb4_general_ci"),
            224 => ("utf8mb4", "utf8mb4_unicode_ci"),
            255 => ("utf8mb4", "utf8mb4_0900_ai_ci"),
            33 => ("utf8", "utf8_general_ci"),
            83 => ("utf8", "utf8_bin"),
            63 => ("binary", "binary"),
            _ => ("utf8mb4", "utf8mb4_bin"),
        };
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
            charset: charset.to_owned(),
            collation: collation.to_owned(),
            results_charset: Some(charset.to_owned()),
            stmts: HashMap::new(),
            timer: Timer::default(),
            mdl: None,
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
    /// Phase timing (`timing`), off by default.
    timer: Timer,
    /// Metadata-lock entry of the current synced statement, released once its response has
    /// been written (`command_done`).
    mdl: Option<StmtGuard>,
}

fn kv_error(e: impl std::fmt::Display) -> SqlError {
    SqlError::unknown(e.to_string())
}

/// A start timestamp requested from PD ahead of the point where it is needed (Go's
/// `txnFuture`): the request is sent when this is created.
struct TsFuture(tokio::task::JoinHandle<tidb_kv::Result<u64>>);

impl TsFuture {
    fn request(kv: &KvClient) -> TsFuture {
        let kv = kv.clone();
        TsFuture(tokio::spawn(async move { kv.current_ts().await }))
    }

    async fn wait(self) -> Result<u64, SqlError> {
        self.0.await.map_err(kv_error)?.map_err(kv_error)
    }
}

/// Whether running the statement will need a start timestamp (a SELECT that reads a table).
fn needs_ts(lowered: &Lowered) -> bool {
    matches!(&lowered.stmt, Statement::Select(sel) if sel.from.is_some())
}

/// Whether the plan is an autocommit point get that reads at MaxTS (Go:
/// `IsPointGetWithPKOrUniqueKeyByAutoCommit`): a handle point get with nothing on top, not
/// on a cached table. Every statement here is autocommit.
fn max_ts_point_get(plan: &tidb_sql::Plan) -> bool {
    matches!(plan.source, tidb_sql::plan::Source::PointGet { .. })
        && plan.agg.is_none()
        && plan.sort.is_empty()
        && plan.limit.is_none()
        && plan.table.cache_table_status == 0
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

    /// Whether statements may currently run in synced mode (no early TSO is requested then).
    fn synced_mode(&self) -> bool {
        self.shared
            .sync
            .as_ref()
            .is_some_and(|s| s.registry().admissible(Instant::now()))
    }

    /// The schema to resolve names with: the synced one, else the one visible at a fresh TSO.
    async fn name_schema(&mut self) -> Result<Arc<InfoSchema>, SqlError> {
        if let Some(is) = self
            .shared
            .sync
            .as_ref()
            .and_then(|s| s.registry().schema_if_admissible(Instant::now()))
        {
            return Ok(is);
        }
        Ok(self.begin_read().await?.1)
    }

    /// A start timestamp and the schema visible at it.
    async fn begin_read(&mut self) -> Result<(u64, Arc<InfoSchema>), SqlError> {
        let ts = self.shared.kv.current_ts().await.map_err(kv_error)?;
        self.timer.mark(Phase::Tso);
        let is = self
            .shared
            .schema
            .info_schema_at(ts)
            .await
            .map_err(kv_error)?;
        self.timer.mark(Phase::Schema);
        Ok((ts, is))
    }

    async fn run(
        &mut self,
        lowered: &Lowered,
        params: &[Literal],
        ts: Option<TsFuture>,
    ) -> Result<Response, SqlError> {
        match &lowered.stmt {
            Statement::Select(sel) => self.select(sel, params, ts).await.map(Response::ResultSet),
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

    async fn select(
        &mut self,
        sel: &Select,
        params: &[Literal],
        ts: Option<TsFuture>,
    ) -> Result<ResultSet, SqlError> {
        if sel.from.is_none() {
            let mut rs = self.const_env().select(sel, params)?;
            for c in &mut rs.columns {
                apply_result_charset(c, self.results_charset.as_deref());
            }
            return Ok(rs);
        }
        let mut ts = ts;
        let admitted = self
            .shared
            .sync
            .as_ref()
            .and_then(|s| s.registry().admit(Instant::now()));
        let synced = match admitted {
            Some(admission) => {
                // A TSO requested before admission (the node entered synced mode in between)
                // may predate the admitted schema's load; take a fresh one instead.
                ts = None;
                self.synced_read(sel, params, admission).await?
            }
            None => None,
        };
        let (plan, rows) = match synced {
            Some(done) => done,
            None => {
                let ts = ts.unwrap_or_else(|| TsFuture::request(&self.shared.kv));
                match self.optimistic_read(sel, params, ts).await? {
                    Ok(done) => done,
                    Err((start_ts, raw)) => self.checked_read(sel, params, start_ts, raw).await?,
                }
            }
        };
        let rows = tidb_executor::finish(&plan, rows, self.shared.dag.new_collation_enabled)?;
        self.timer.mark(Phase::Finish);
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

    fn plan(
        &self,
        sel: &Select,
        is: &InfoSchema,
        params: &[Literal],
    ) -> Result<tidb_sql::Plan, SqlError> {
        tidb_sql::plan_select(
            sel,
            is,
            PlanContext {
                current_db: self.current_db.as_deref(),
                params,
                describe_only: false,
            },
        )
    }

    /// Synced mode (ADR 0005): plan with the newest loaded schema, narrow the statement's
    /// metadata-lock entry to the table, and read at MaxTS (point gets) or at a fresh TSO.
    /// `None` if the node left sync before the read finished: the result may come from data
    /// two schema versions ahead, so it is discarded and the caller takes the fallback path.
    #[allow(clippy::type_complexity)]
    async fn synced_read(
        &mut self,
        sel: &Select,
        params: &[Literal],
        admission: Admission,
    ) -> Result<Option<(tidb_sql::Plan, Vec<Vec<tidb_distsql::Datum>>)>, SqlError> {
        let Admission { schema, guard } = admission;
        let plan = self.plan(sel, &schema, params)?;
        guard.set_tables(vec![plan.table.id]);
        self.timer.set_kind(Kind::of(&plan));
        self.timer.mark(Phase::Plan);
        let mut snapshot = if max_ts_point_get(&plan) {
            self.shared.kv.latest_snapshot()
        } else {
            let start_ts = TsFuture::request(&self.shared.kv).wait().await?;
            self.timer.mark(Phase::Tso);
            self.shared.kv.snapshot(start_ts)
        };
        let cx = ExecContext {
            dag: self.shared.dag.clone(),
            schema_version: schema.version(),
        };
        let rows = tidb_executor::read_source(&plan, &mut snapshot, &cx).await;
        self.timer.mark(Phase::KvRead);
        if !guard.results_valid() {
            return Ok(None);
        }
        self.mdl = Some(guard);
        Ok(Some((plan, rows?)))
    }

    /// Plan with the newest cached schema and read at `start_ts` while checking, concurrently
    /// and at the same `start_ts`, that this schema is the one visible there. Returns the plan
    /// and source rows if the check passed (read errors are returned only then), else
    /// `Err((start_ts, raw version key if read))` for [`Session::checked_read`].
    #[allow(clippy::type_complexity)]
    async fn optimistic_read(
        &mut self,
        sel: &Select,
        params: &[Literal],
        ts: TsFuture,
    ) -> Result<Result<(tidb_sql::Plan, Vec<Vec<tidb_distsql::Datum>>), (u64, Option<i64>)>, SqlError>
    {
        let planned = self
            .shared
            .schema
            .candidate()
            .and_then(|is| self.plan(sel, &is, params).ok().map(|plan| (is, plan)));
        self.timer.mark(Phase::Plan);
        let start_ts = ts.wait().await?;
        self.timer.mark(Phase::Tso);
        // No cached schema yet, or planning failed with it (it may not be the right one):
        // take the checked path, which reports genuine planning errors.
        let Some((is, plan)) = planned else {
            return Ok(Err((start_ts, None)));
        };
        self.timer.set_kind(Kind::of(&plan));
        let mut snapshot = self.shared.kv.snapshot(start_ts);
        let cx = ExecContext {
            dag: self.shared.dag.clone(),
            schema_version: is.version(),
        };
        let (check, rows) = futures::join!(
            self.shared.schema.check_at(&is, start_ts),
            tidb_executor::read_source(&plan, &mut snapshot, &cx)
        );
        self.timer.mark(Phase::KvRead);
        match check.map_err(kv_error)? {
            SchemaCheck::Valid => Ok(Ok((plan, rows?))),
            SchemaCheck::Stale { raw } => Ok(Err((start_ts, raw))),
        }
    }

    /// The statement at `start_ts` with the schema visible there, as `begin_read` does
    /// (`raw`: the version key at `start_ts`, if already read).
    async fn checked_read(
        &mut self,
        sel: &Select,
        params: &[Literal],
        start_ts: u64,
        raw: Option<i64>,
    ) -> Result<(tidb_sql::Plan, Vec<Vec<tidb_distsql::Datum>>), SqlError> {
        let schema = &self.shared.schema;
        let is = match raw {
            Some(raw) => schema.info_schema_with_raw(start_ts, raw).await,
            None => schema.info_schema_at(start_ts).await,
        }
        .map_err(kv_error)?;
        self.timer.mark(Phase::Schema);
        let plan = self.plan(sel, &is, params)?;
        self.timer.set_kind(Kind::of(&plan));
        self.timer.mark(Phase::Plan);
        let mut snapshot = self.shared.kv.snapshot(start_ts);
        let cx = ExecContext {
            dag: self.shared.dag.clone(),
            schema_version: is.version(),
        };
        let rows = tidb_executor::read_source(&plan, &mut snapshot, &cx).await?;
        self.timer.mark(Phase::KvRead);
        Ok((plan, rows))
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
        self.timer.begin();
        self.mdl = None;
        let lowered = tidb_sql::parse_and_lower(sql)?;
        self.timer.mark(Phase::Parse);
        if lowered.param_count > 0 {
            return Err(SqlError::new(
                1064,
                "You have an error in your SQL syntax; '?' is only allowed in prepared statements",
            ));
        }
        let ts =
            (needs_ts(&lowered) && !self.synced_mode()).then(|| TsFuture::request(&self.shared.kv));
        self.run(&lowered, &[], ts).await
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
                let is = self.name_schema().await?;
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
        self.timer.begin();
        self.mdl = None;
        let ts =
            (needs_ts(&lowered) && !self.synced_mode()).then(|| TsFuture::request(&self.shared.kv));
        let params = params_to_literals(params)?;
        self.timer.mark(Phase::Parse);
        self.run(&lowered, &params, ts).await
    }

    fn command_done(&mut self, write: std::time::Duration, total: std::time::Duration) {
        self.mdl = None;
        self.timer.end(write, total);
    }

    fn close(&mut self, stmt_id: u32) {
        self.stmts.remove(&stmt_id);
    }

    async fn use_db(&mut self, db: &str) -> Result<(), SqlError> {
        let is = self.name_schema().await?;
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
