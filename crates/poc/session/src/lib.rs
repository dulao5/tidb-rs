//! SQL sessions of the read-only node: implements the protocol layer's [`SessionFactory`] /
//! [`SessionHandler`] on top of the planner (`tidb-sql`) and executors (`tidb-executor`).
//!
//! **Synced mode** (`--schema-sync`, ADR 0005): while the node is registered and in sync with
//! the cluster (`poc_domain::Registry::admit`), a SELECT uses the newest schema loaded in the
//! background and holds a metadata-lock entry until its response is written. Autocommit point
//! gets by handle read at MaxTS without a TSO; other reads take a TSO but read no schema
//! version: the schema lease (Go's `SchemaValidator`) bounds how old the cached schema may be
//! at the read TS, so a slow reload does not send statements to the meta region (#136). If
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
pub mod plan_cache;
pub mod privilege;
pub mod router;
pub mod timing;

use std::cell::Cell;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use poc_domain::Admission;
use poc_domain::ResultCheck;
use poc_domain::SchemaSync;
use poc_domain::StmtGuard;
use poc_executor::ExecContext;
use poc_kv::KvClient;
use tidb_base::contextutil::SQLWarn;
use tidb_distsql::DagContext;
use tidb_meta::InfoSchema;
use tidb_meta::SchemaCache;
use tidb_meta::SchemaCheck;
use tidb_metrics::ExpensiveExec;
use tidb_metrics::StmtType;
use tidb_mysql_protocol::AccountAuth;
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
use crate::router::NewStackMode;
use crate::router::PocValues;
use crate::router::Router;
use crate::timing::Kind;
use crate::timing::Phase;
use crate::timing::Timer;

/// State shared by all sessions.
pub struct Shared {
    pub kv: KvClient,
    pub schema: SchemaCache,
    pub dag: DagContext,
    /// Fallback single account used only when `privileges` is `None` (unit tests without cluster
    /// `mysql.*` privilege tables).
    pub user: String,
    pub credential: Credential,
    pub server_version: String,
    pub tls_enabled: bool,
    /// Schema sync (ADR 0005); `None` with `--schema-sync=off`.
    pub sync: Option<SchemaSync>,
    /// Cluster-loaded `mysql.*` privilege cache (D04.1 Part 5/5). When `Some`, drives multi-account
    /// login authentication and session-level privilege enforcement on both stacks.
    pub privileges: Option<privilege::PrivilegeSource>,
}

/// Creates [`Session`]s.
#[derive(Clone)]
pub struct Factory {
    shared: Arc<Shared>,
    /// `--new-stack` (VS1 plan §4.3).
    new_stack: NewStackMode,
    /// The ported store driver the new stack's sessions read through (`None` with
    /// `--new-stack=off`: nothing of the new stack is built).
    new_store: Option<Arc<tidb_store::driver::TikvStore>>,
    /// The sysvar cache of `mysql.global_variables` the sessions read their global values
    /// from (the loader's, `poc_domain::sysvar_loader`; D06.5 spec §8), in every mode.
    /// `None` without a loader: the new sessions' global values are unknown and they stay
    /// on the PoC (step 0, "global variables not loaded"); with `off`, the plan-cache
    /// decision uses the compiled defaults.
    new_globals: Option<Arc<tidb_systable::sysvar_cache::SysVarCache>>,
}

impl Factory {
    pub fn new(shared: Shared) -> Factory {
        Factory {
            shared: Arc::new(shared),
            new_stack: NewStackMode::Off,
            new_store: None,
            new_globals: None,
        }
    }

    /// Sessions route statements the ported stack supports to it (`fallback` / `strict`, [`router`]); with
    /// `off` (the default) they run everything on the PoC, as before. The new stack reads
    /// through the ported store driver over the PoC's TiKV client.
    pub fn with_new_stack(mut self, mode: NewStackMode) -> Result<Factory, String> {
        self.new_stack = mode;
        if mode != NewStackMode::Off {
            let store = self
                .shared
                .kv
                .store_driver()
                .map_err(|e| format!("the store driver of the new stack: {e}"))?;
            self.new_store = Some(Arc::new(store));
        }
        Ok(self)
    }

    /// The sessions read their global variables from `cache`, the sysvar cache the loader
    /// keeps in step with `mysql.global_variables` (D06.5 spec §8).
    pub fn with_sys_var_cache(
        mut self,
        cache: Arc<tidb_systable::sysvar_cache::SysVarCache>,
    ) -> Factory {
        self.new_globals = Some(cache);
        self
    }

    /// The state shared by this factory's sessions.
    pub fn shared(&self) -> &Shared {
        &self.shared
    }
}

impl SessionFactory for Factory {
    type Session = Session;

    async fn lookup_user(&self, user: &str, host: &str) -> Option<Credential> {
        self.lookup_account(user, host).await.map(|a| a.credential)
    }

    async fn lookup_account(&self, user: &str, host: &str) -> Option<AccountAuth> {
        let Some(privs) = &self.shared.privileges else {
            return (user == self.shared.user).then(|| AccountAuth {
                credential: self.shared.credential.clone(),
                auth_user: Some(self.shared.user.clone()),
                auth_host: Some("%".to_owned()),
                account_locked: false,
                password_expired: false,
            });
        };
        // Read directly from the in-memory cache (same as Go's `ConnectionVerification`).
        // Never trigger an unauthenticated KV scan on miss; privilege updates arrive via the
        // etcd `/tidb/privilege` watch and periodic background refresh loop.
        let snap = privs.handle().get();
        privilege::authenticate_user_record(&snap, user, host)
    }

    fn open_session(&self, info: &ConnectionInfo) -> Result<Session, SqlError> {
        let host = {
            let h = info
                .peer
                .rsplit_once(':')
                .map_or(info.peer.as_str(), |(h, _)| h)
                .trim_matches(|c| c == '[' || c == ']');
            if let Some(v4) = h
                .strip_prefix("::ffff:")
                .or_else(|| h.strip_prefix("::FFFF:"))
                .filter(|s| s.parse::<std::net::Ipv4Addr>().is_ok())
            {
                v4
            } else {
                h
            }
        };
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
        let (auth_user, auth_host, active_roles, privilege_manager) =
            if let Some(privs) = &self.shared.privileges {
                // Bind the exact `(auth_user, auth_host)` matched during `authenticate` with no
                // TOCTOU re-match and no `(user, "%")` fallback; fail closed if absent.
                let (Some(au), Some(ah)) = (&info.auth_user, &info.auth_host) else {
                    return Err(SqlError::access_denied(&info.user, host, false));
                };
                let snap = privs.handle().get();
                let roles = snap.get_default_roles(au, ah);
                let pm: tidb_privilege::PrivilegeManagerRef =
                    Arc::new(tidb_privilege::UserPrivileges::new(
                        au.clone(),
                        ah.clone(),
                        Arc::clone(privs.handle()),
                    ));
                (au.clone(), ah.clone(), roles, Some(pm))
            } else {
                (info.user.clone(), "%".to_owned(), Vec::new(), None)
            };
        let mut session = Session {
            shared: self.shared.clone(),
            current_db: None,
            user: format!("{}@{host}", info.user),
            current_user: format!("{auth_user}@{auth_host}"),
            auth_user: auth_user.clone(),
            auth_host: auth_host.clone(),
            active_roles: active_roles.clone(),
            privilege_manager: privilege_manager.clone(),
            connection_id: info.connection_id,
            charset: charset.to_owned(),
            collation: collation.to_owned(),
            results_charset: Some(charset.to_owned()),
            stmts: HashMap::new(),
            warnings: Vec::new(),
            default_vars: None,
            globals: self.new_globals.clone(),
            default_vars_loaded: false,
            timer: Timer::default(),
            mdl: None,
            stmt: StmtMetrics::default(),
            router: None,
            prev_stmt: router::LastStmtInfo::default(),
        };
        if self.new_stack != NewStackMode::Off {
            let domain = self
                .shared
                .sync
                .as_ref()
                .and_then(|s| s.ported_domain().cloned());
            let mut new = Box::new(router::newstack::NewSession::new(
                domain,
                self.new_store.clone(),
                self.shared.kv.exec_pool(),
                // Without a loader the cache is never built: the global values are unknown,
                // so the router keeps the session on the PoC.
                self.new_globals
                    .clone()
                    .unwrap_or_else(|| Arc::new(tidb_systable::sysvar_cache::SysVarCache::new())),
            ));
            if let Some(pm) = privilege_manager {
                let ns = router::NewStack::session_mut(&mut *new);
                ns.vars_mut().user = Some(tidb_parser::auth::UserIdentity {
                    username: info.user.clone(),
                    hostname: host.to_owned(),
                    auth_username: auth_user,
                    auth_hostname: auth_host,
                    ..Default::default()
                });
                ns.vars_mut().active_roles = active_roles;
                ns.set_privilege_manager(Some(pm));
            }
            let mut r = Router::new(self.new_stack, new, &session.poc_values());
            r.set_connection_id(session.connection_id);
            session.router = Some(Box::new(r));
        }
        Ok(session)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        router::trace::forget(self.connection_id);
    }
}

/// One client connection's session.
pub struct Session {
    shared: Arc<Shared>,
    /// Current database as given to `USE` (Go `CurrentDB`).
    current_db: Option<String>,
    user: String,
    current_user: String,
    auth_user: String,
    auth_host: String,
    active_roles: Vec<tidb_parser::auth::RoleIdentity>,
    privilege_manager: Option<tidb_privilege::PrivilegeManagerRef>,
    connection_id: u64,
    /// `SET NAMES` charset / collation (`character_set_client`/`connection`).
    charset: String,
    collation: String,
    /// `@@character_set_results` (`None` = NULL: columns keep their own charset).
    results_charset: Option<String>,
    /// The previous statement's state as the router last handed it over (VS1 plan §4.5):
    /// what `@@last_plan_from_cache` and `@@last_plan_from_binding` read.
    prev_stmt: router::LastStmtInfo,
    /// Prepared statements by id.
    stmts: HashMap<u32, PocStmt>,
    /// The warnings of the statement the prototype ran last (Go's `StmtCtx` warnings): the
    /// plan cache's `skip prepared plan-cache` warning (#512, [`plan_cache`]) is the only
    /// one the prototype reports. Cleared when a statement other than SHOW WARNINGS / SHOW
    /// ERRORS starts, as Go's `ResetContextOfStmt` does.
    warnings: Vec<SQLWarn>,
    /// With `--new-stack=off` (no router, no new session): a session with Go's default
    /// variables, the input of a PREPARE's plan-cache decision ([`plan_cache`]); made at the
    /// first PREPARE.
    default_vars: Option<Box<tidb_session::Session>>,
    /// The sysvar cache `default_vars` takes the cluster's global values from (the
    /// factory's; D06.5 spec §8).
    globals: Option<Arc<tidb_systable::sysvar_cache::SysVarCache>>,
    /// `default_vars` holds the cluster's global values (not the compiled defaults).
    default_vars_loaded: bool,
    /// Phase timing (`timing`), off by default.
    timer: Timer,
    /// Metadata-lock entry of the current synced statement, released once its response has
    /// been written (`command_done`).
    mdl: Option<StmtGuard>,
    /// Prometheus bookkeeping of the statement in flight.
    stmt: StmtMetrics,
    /// The router between the PoC and the ported stack (`--new-stack=fallback|strict`); `None` with `off`.
    router: Option<Box<Router>>,
}

/// A statement the prototype prepared.
struct PocStmt {
    lowered: Arc<Lowered>,
    /// Go's `PlanCacheStmt.UncacheableReason` ([`plan_cache`]): warned on every EXECUTE.
    plan_cache_reason: String,
}

/// What the Go session metrics need to know about the statement in flight: its type and how
/// its time splits into parse / compile / execute (Go `SessionVars.DurationParse`,
/// `DurationCompile`, `GetExecuteDuration`), and whether it ran in a transaction with a start
/// timestamp (Go records `tidb_session_transaction_duration_seconds` when such an autocommit
/// transaction commits; MaxTS point gets never activate one).
#[derive(Default)]
struct StmtMetrics {
    /// A statement got past parsing; cleared once recorded.
    active: bool,
    stmt_type: Option<StmtType>,
    parse: Duration,
    /// Planning time (a `Cell`: `plan` takes `&self`).
    compile: Cell<Duration>,
    took_ts: bool,
    failed: bool,
}

impl StmtMetrics {
    /// A statement of type `t` starts executing after `parse` (`COM_QUERY`: parsing,
    /// `COM_STMT_EXECUTE`: decoding parameters, which Go does not count as parsing).
    fn start(&mut self, t: StmtType, parse: Duration) {
        if self.active {
            // Started already in this command: the VS1 router started it for the new stack
            // and the statement fell back to the PoC. Never the case with `--new-stack=off`.
            return;
        }
        *self = StmtMetrics {
            active: true,
            stmt_type: Some(t),
            parse,
            ..StmtMetrics::default()
        };
        tidb_metrics::statement(t);
    }

    /// The statement's response was written; `total` is the whole command (Go: total cost).
    fn finish(&mut self, total: Duration) {
        if !std::mem::take(&mut self.active) {
            return;
        }
        let compile = self.compile.get();
        tidb_metrics::compile_duration(compile);
        tidb_metrics::execute_duration(total.saturating_sub(self.parse + compile));
        if self.took_ts && !self.failed {
            tidb_metrics::transaction(total.saturating_sub(self.parse), true);
        }
        if total >= tidb_metrics::SLOW_THRESHOLD {
            tidb_metrics::slow_query(total);
        }
    }
}

/// Go `ast.GetStmtLabel`.
pub(crate) fn stmt_type(stmt: &Statement) -> StmtType {
    match stmt {
        Statement::Select(_) => StmtType::Select,
        Statement::Use(_) => StmtType::Use,
        Statement::Set(_) => StmtType::Set,
        Statement::Begin => StmtType::Begin,
        Statement::Commit => StmtType::Commit,
        Statement::Rollback => StmtType::Rollback,
        Statement::ShowWarnings { .. } => StmtType::Show,
    }
}

/// The executors Go's builder counts in `tidb_executor_expensive_total` for this plan: the root
/// aggregation (`HashAgg` with GROUP BY, `StreamAgg` for a scalar aggregate over a pushed
/// partial one, as Go plans the sysbench statements) and the root sort (`TopN` with a limit).
fn count_expensive(plan: &tidb_sql::Plan) {
    if let Some(agg) = &plan.agg {
        tidb_metrics::expensive(if agg.group_by.is_empty() {
            ExpensiveExec::StreamAgg
        } else {
            ExpensiveExec::HashAgg
        });
    }
    if !plan.sort.is_empty() {
        tidb_metrics::expensive(if plan.limit.is_some() {
            ExpensiveExec::TopN
        } else {
            ExpensiveExec::Sort
        });
    }
}

fn kv_error(e: impl std::fmt::Display) -> SqlError {
    SqlError::unknown(e.to_string())
}

/// A start timestamp requested from PD ahead of the point where it is needed (Go's
/// `txnFuture`): the request is sent when this is created.
struct TsFuture(poc_kv::TsFuture);

impl TsFuture {
    fn request(kv: &KvClient) -> TsFuture {
        TsFuture(kv.ts_future())
    }

    async fn wait(self) -> Result<u64, SqlError> {
        self.0.wait().await.map_err(kv_error)
    }
}

/// `?` in a `COM_QUERY` statement.
pub(crate) fn param_in_query() -> SqlError {
    SqlError::new(
        1064,
        "You have an error in your SQL syntax; '?' is only allowed in prepared statements",
    )
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

/// The fence of explicit transactions (`specs/exceptions/fences.md`): the read-only node runs
/// every statement in autocommit mode, so BEGIN / START TRANSACTION, COMMIT and ROLLBACK answer
/// 1235. `None` for any other statement.
fn txn_fence(stmt: &Statement) -> Option<SqlError> {
    match stmt {
        Statement::Begin => Some(not_in_txn("BEGIN/START TRANSACTION")),
        Statement::Commit => Some(not_in_txn("COMMIT")),
        Statement::Rollback => Some(not_in_txn("ROLLBACK")),
        _ => None,
    }
}

/// `SET autocommit = value`: on is accepted, off is the explicit-transaction fence (1235), a
/// value that is not a boolean is 1231.
fn autocommit_fence(value: &SetValue) -> Result<(), SqlError> {
    match bool_value(value) {
        Some(true) => Ok(()),
        Some(false) => Err(not_in_txn("autocommit=0")),
        None => Err(SqlError::new(
            1231,
            "Variable 'autocommit' can't be set to the value given",
        )),
    }
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
            last_plan_from_cache: self.prev_stmt.plan_from_cache,
            last_plan_from_binding: self.prev_stmt.plan_from_binding,
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

    /// Runs a statement; the response carries the statement's warning count, as Go's OK / EOF
    /// packets do (`StmtCtx.WarningCount()`).
    async fn run(
        &mut self,
        lowered: &Lowered,
        params: &[Literal],
        ts: Option<TsFuture>,
    ) -> Result<Response, SqlError> {
        let mut r = self.run_stmt(lowered, params, ts).await;
        let n = u16::try_from(self.warnings.len()).unwrap_or(u16::MAX);
        match &mut r {
            Ok(Response::ResultSet(rs)) => rs.warnings = n,
            Ok(Response::Encoded(rs)) => rs.warnings = n,
            Ok(Response::Ok(ok)) => ok.warnings = n,
            Err(_) => {}
        }
        r
    }

    /// A statement starts: Go's `ResetContextOfStmt` drops the previous statement's warnings,
    /// except for SHOW WARNINGS / SHOW ERRORS, which show them.
    fn reset_warnings(&mut self, stmt: &Statement) {
        if !matches!(stmt, Statement::ShowWarnings { .. }) {
            self.warnings.clear();
        }
    }

    async fn run_stmt(
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
            Statement::Begin | Statement::Commit | Statement::Rollback => {
                Err(txn_fence(&lowered.stmt).unwrap_or_else(|| not_in_txn("this statement")))
            }
            Statement::ShowWarnings { errors } => Ok(Response::ResultSet(warnings_result_set(
                &self.warnings,
                *errors,
            ))),
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
        count_expensive(&plan);
        let rows = poc_executor::finish(&plan, rows, self.shared.dag.new_collation_enabled)?;
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
        let start = Instant::now();
        // Match Go's `preprocess -> build_logical_plan -> check_privilege` order:
        // 1. Preprocess R31 missing-table check (1142 before 1146).
        if let Some(pm) = &self.privilege_manager {
            privilege::check_poc_missing_table_privilege(
                pm.as_ref(),
                &self.active_roles,
                &self.auth_user,
                &self.auth_host,
                self.current_db.as_deref(),
                sel,
                is,
            )?;
        }
        // 2. Plan build (unknown columns, only_full_group_by, unsupported SQL errors come first).
        let plan = tidb_sql::plan_select(
            sel,
            is,
            PlanContext {
                current_db: self.current_db.as_deref(),
                params,
                describe_only: false,
            },
        );
        let compile = &self.stmt.compile;
        compile.set(compile.get() + start.elapsed());
        let plan = plan?;
        // 3. Post-build privilege verification (R29/R30/R32).
        if let Some(pm) = &self.privilege_manager {
            privilege::check_poc_select_privilege(
                pm.as_ref(),
                &self.active_roles,
                &self.auth_user,
                &self.auth_host,
                self.current_db.as_deref(),
                sel,
                is,
            )?;
        }
        Ok(plan)
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
        let (mut snapshot, start_ts) = if max_ts_point_get(&plan) {
            (self.shared.kv.latest_snapshot(), None)
        } else {
            let start_ts = TsFuture::request(&self.shared.kv).wait().await?;
            self.stmt.took_ts = true;
            self.timer.mark(Phase::Tso);
            (self.shared.kv.snapshot(start_ts), Some(start_ts))
        };
        let cx = ExecContext {
            dag: self.shared.dag.clone(),
            schema_version: schema.version(),
        };
        let rows = poc_executor::read_source(&plan, &mut snapshot, &cx).await;
        self.timer.mark(Phase::KvRead);
        // Fencing, then the schema lease at the read TS (Go: `SchemaChecker.Check`).
        match guard.check_results(start_ts).await {
            ResultCheck::Valid => {}
            ResultCheck::Fallback => return Ok(None),
            ResultCheck::Err(e) => {
                let msg = e.to_string();
                let src = tidb_base::Error::plain(format!("[domain:{}]{msg}", e.code())).trace();
                return Err(SqlError::new(e.code(), msg).with_source_err(src));
            }
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
        self.stmt.took_ts = true;
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
            poc_executor::read_source(&plan, &mut snapshot, &cx)
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
        let rows = poc_executor::read_source(&plan, &mut snapshot, &cx).await?;
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
                        "autocommit" => autocommit_fence(value)?,
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

/// The rows of `SHOW WARNINGS` (`errors`: `SHOW ERRORS`, only the errors) for `warns`, as
/// Go's `ShowExec.fetchShowWarnings` makes them: a TiDB error is shown with its MySQL code
/// and message (`terror.ToSQLError`), any other error with `ErrUnknown` and its text. Used for
/// the prototype's own warnings and, by the router, for the new session's.
// glue: rows as Go ShowExec.fetchShowWarnings (the prototype's SHOW WARNINGS)
pub(crate) fn warnings_result_set(warns: &[SQLWarn], errors: bool) -> ResultSet {
    let mut rs = show_warnings_columns();
    for w in warns {
        if errors && w.level != tidb_base::contextutil::WarnLevelError {
            continue;
        }
        let (code, message) = if w.err.is_normalized() {
            let sql_err = tidb_base::terror::to_sql_error(&w.err);
            (i64::from(sql_err.code), sql_err.message)
        } else {
            (i64::from(tidb_base::mysql::ErrUnknown), w.err.error())
        };
        rs.rows.push(vec![
            Value::Bytes(w.level.as_bytes().to_vec()),
            Value::Int(code),
            Value::Bytes(message),
        ]);
    }
    rs
}

/// The columns of `SHOW WARNINGS` (Go's), with no rows.
fn show_warnings_columns() -> ResultSet {
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
        let r = match self.router.take() {
            None => {
                router::trace::record(self.connection_id, "poc", "new_stack_off");
                self.query_inner(sql).await
            }
            Some(mut router) => {
                let r = router.query(self, sql).await;
                self.router = Some(router);
                r
            }
        };
        self.stmt.failed = r.is_err();
        r
    }

    async fn prepare(&mut self, stmt_id: u32, sql: &str) -> Result<PreparedInfo, SqlError> {
        self.stmt.stmt_type = None;
        match self.router.take() {
            None => self.prepare_inner(stmt_id, sql).await,
            Some(mut router) => {
                let r = router.prepare(self, stmt_id, sql).await;
                self.router = Some(router);
                r
            }
        }
    }

    async fn execute(&mut self, stmt_id: u32, params: Vec<Param>) -> Result<Response, SqlError> {
        let r = match self.router.take() {
            None => {
                router::trace::record(self.connection_id, "poc", "new_stack_off");
                self.execute_inner(stmt_id, params).await
            }
            Some(mut router) => {
                let r = router.execute(self, stmt_id, params).await;
                self.router = Some(router);
                r
            }
        };
        self.stmt.failed = r.is_err();
        r
    }

    fn execute_flags(&mut self, _stmt_id: u32, flags: u8) {
        if let Some(router) = &mut self.router {
            router.set_execute_flags(flags);
        }
    }

    fn execute_raw_params(&mut self, _stmt_id: u32, raw: tidb_mysql_protocol::RawParams) {
        if let Some(router) = &mut self.router {
            router.set_execute_raw_params(raw);
        }
    }

    fn command_done(&mut self, write: std::time::Duration, total: std::time::Duration) {
        self.mdl = None;
        self.timer.end(write, total);
        self.stmt.finish(total);
    }

    fn stmt_type(&self) -> StmtType {
        self.stmt.stmt_type.unwrap_or(StmtType::General)
    }

    fn close(&mut self, stmt_id: u32) {
        match self.router.take() {
            None => {
                self.stmts.remove(&stmt_id);
            }
            Some(mut router) => {
                router.close(self, stmt_id);
                self.router = Some(router);
            }
        }
    }

    async fn use_db(&mut self, db: &str) -> Result<(), SqlError> {
        // COM_INIT_DB and the handshake's database: Go runs them as a `USE` statement.
        tidb_metrics::statement(StmtType::Use);
        let r = self.use_db_inner(db).await;
        if r.is_ok()
            && let Some(router) = self.router.take()
        {
            let mut router = router;
            router.use_db(self);
            self.router = Some(router);
        }
        r
    }

    fn status_flags(&self) -> u16 {
        if let Some(router) = &self.router {
            router.new_stack().session().vars().status()
        } else {
            tidb_mysql_protocol::constants::status::AUTOCOMMIT
        }
    }

    fn session_alias(&self) -> &str {
        if let Some(router) = &self.router {
            &router.new_stack().session().vars().session_alias
        } else {
            ""
        }
    }

    fn txn_mode(&self) -> &str {
        if let Some(router) = &self.router {
            let m = &router.new_stack().session().vars().txn_mode;
            if m.is_empty() {
                "OPTIMISTIC"
            } else {
                m.as_str()
            }
        } else {
            "PESSIMISTIC"
        }
    }

    fn txn_timestamp(&self) -> u64 {
        if let Some(router) = &self.router {
            let tc = &router.new_stack().session().vars().state.txn_ctx;
            if tc.start_ts != 0 {
                tc.start_ts
            } else {
                tc.stale_read_ts
            }
        } else {
            0
        }
    }

    fn redact_mode(&self) -> &str {
        if let Some(router) = &self.router {
            &router.new_stack().session().vars().enable_redact_log
        } else {
            tidb_base::errors::redact_log_enabled_load()
        }
    }

    fn collation_name(&self) -> &str {
        if !self.collation.is_empty() {
            &self.collation
        } else {
            "utf8mb4_bin"
        }
    }
}

impl Session {
    async fn query_inner(&mut self, sql: &str) -> Result<Response, SqlError> {
        self.timer.begin();
        self.mdl = None;
        self.stmt.stmt_type = None;
        let start = Instant::now();
        let lowered = match tidb_sql::parse_and_lower(sql) {
            Ok(l) => l,
            Err(e) => {
                if e.code == tidb_base::errno::ErrParse {
                    // Go fails before `ResetContextOfStmt`: the warnings stay.
                    return Err(router::log_and_wrap_parse_error(
                        self.connection_id,
                        "",
                        tidb_base::errors::redact_log_enabled_load(),
                        sql,
                        e,
                    ));
                }
                // A statement the prototype does not lower: Go would have started it.
                self.warnings.clear();
                return Err(e);
            }
        };
        let parse = start.elapsed();
        tidb_metrics::parse_duration(parse);
        self.stmt.start(stmt_type(&lowered.stmt), parse);
        self.timer.mark(Phase::Parse);
        self.reset_warnings(&lowered.stmt);
        if lowered.param_count > 0 {
            return Err(param_in_query());
        }
        self.run_query(&lowered).await
    }

    /// A `COM_QUERY` statement after parsing: request the TSO early, then run it.
    async fn run_query(&mut self, lowered: &Lowered) -> Result<Response, SqlError> {
        self.reset_warnings(&lowered.stmt);
        let ts =
            (needs_ts(lowered) && !self.synced_mode()).then(|| TsFuture::request(&self.shared.kv));
        self.run(lowered, &[], ts).await
    }

    /// The state the VS1 router syncs into the new session.
    fn poc_values(&self) -> PocValues {
        PocValues {
            current_db: self.current_db.clone(),
            charset: self.charset.clone(),
            collation: self.collation.clone(),
            results_charset: self.results_charset.clone(),
        }
    }

    /// `COM_STMT_PREPARE` with `--new-stack=off`: prepare on the prototype, then take Go's
    /// prepare-time plan-cache decision with Go's default variables and no InfoSchema
    /// ([`plan_cache`], #512).
    async fn prepare_inner(&mut self, stmt_id: u32, sql: &str) -> Result<PreparedInfo, SqlError> {
        // A parse error comes before Go's `ResetContextOfStmt` (`PrepareExec.Next`): the
        // warnings stay.
        let stmt = tidb_sql::ir::parse_one(sql)?;
        let lowered = match tidb_sql::ir::lower_statement(&stmt, sql) {
            Ok(l) => l,
            Err(e) => {
                self.warnings.clear();
                return Err(e);
            }
        };
        let info = self.prepare_lowered(stmt_id, lowered).await?;
        let decision = match self.default_vars() {
            Ok(vars) => plan_cache::prepare_time_cacheability(vars, &stmt, None),
            Err(e) => Err(e),
        };
        match decision {
            Ok(c) => {
                router::PocEngine::set_cacheability(self, stmt_id, c);
                Ok(info)
            }
            Err(e) => {
                self.stmts.remove(&stmt_id);
                Err(e)
            }
        }
    }

    /// The variables of the off-mode plan-cache decision: a new session's, with the
    /// cluster's global values once the sysvar cache is built. Before that, the compiled
    /// defaults, logged once per process (maintainer decision on #521, D06.5 spec §8.3); a
    /// session made from them is made again once the cache is built (as the router loads a
    /// new session's values late, #539 review).
    fn default_vars(
        &mut self,
    ) -> Result<&tidb_sessionctx::variable::session::SessionVars, SqlError> {
        let built = self.globals.as_ref().is_some_and(|c| !c.needs_rebuild());
        if built && !self.default_vars_loaded {
            self.default_vars = None;
        }
        if self.default_vars.is_none() {
            self.default_vars_loaded = built;
            let globals = match &self.globals {
                Some(cache) if built => router::newstack::ClusterGlobals::new(Arc::clone(cache)),
                _ => {
                    static WARNED: std::sync::Once = std::sync::Once::new();
                    WARNED.call_once(|| {
                        tidb_base::log_warn!(
                            "the global variables of the cluster are not loaded yet (mysql.global_variables); the prototype uses the compiled defaults until they are"
                        );
                    });
                    router::newstack::ClusterGlobals::compiled_defaults()
                }
            };
            let mut s = tidb_session::Session::new(tidb_session::Dialect::MySQL, Box::new(globals));
            s.load_common_global_variables_if_needed()
                .map_err(|e| router::sql_error(&e))?;
            self.default_vars = Some(Box::new(s));
        }
        Ok(self.default_vars.as_ref().expect("set above").vars())
    }

    async fn prepare_lowered(
        &mut self,
        stmt_id: u32,
        lowered: Lowered,
    ) -> Result<PreparedInfo, SqlError> {
        self.reset_warnings(&lowered.stmt);
        let param_count = u16::try_from(lowered.param_count).map_err(|_| {
            SqlError::new(1390, "Prepared statement contains too many placeholders")
        })?;
        let mut columns = Vec::new();
        if let Statement::Select(sel) = &lowered.stmt {
            if sel.from.is_some() {
                // Resolve names now, like Go's prepare: preprocess missing-table R31 check,
                // then plan build (unknown tables/columns & semantic checks), then privilege check.
                let is = self.name_schema().await?;
                if let Some(pm) = &self.privilege_manager {
                    privilege::check_poc_missing_table_privilege(
                        pm.as_ref(),
                        &self.active_roles,
                        &self.auth_user,
                        &self.auth_host,
                        self.current_db.as_deref(),
                        sel,
                        &is,
                    )?;
                }
                let plan = tidb_sql::plan_select(
                    sel,
                    &is,
                    PlanContext {
                        current_db: self.current_db.as_deref(),
                        params: &[],
                        describe_only: true,
                    },
                )?;
                if let Some(pm) = &self.privilege_manager {
                    privilege::check_poc_select_privilege(
                        pm.as_ref(),
                        &self.active_roles,
                        &self.auth_user,
                        &self.auth_host,
                        self.current_db.as_deref(),
                        sel,
                        &is,
                    )?;
                }
                columns = plan.columns;
                for c in &mut columns {
                    apply_result_charset(c, self.results_charset.as_deref());
                }
            } else if param_count == 0 {
                columns = self.const_env().select(sel, &[])?.columns;
            }
        }
        self.stmts.insert(
            stmt_id,
            PocStmt {
                lowered: Arc::new(lowered),
                plan_cache_reason: String::new(),
            },
        );
        Ok(PreparedInfo {
            param_count,
            columns,
            ..Default::default()
        })
    }

    async fn execute_inner(
        &mut self,
        stmt_id: u32,
        params: Vec<Param>,
    ) -> Result<Response, SqlError> {
        self.stmt.stmt_type = None;
        let (lowered, reason) = self
            .stmts
            .get(&stmt_id)
            .map(|s| (Arc::clone(&s.lowered), s.plan_cache_reason.clone()))
            .ok_or_else(|| SqlError::unknown_stmt(stmt_id, "stmt_execute"))?;
        self.timer.begin();
        self.mdl = None;
        let start = Instant::now();
        let ts =
            (needs_ts(&lowered) && !self.synced_mode()).then(|| TsFuture::request(&self.shared.kv));
        let params = params_to_literals(params)?;
        self.stmt.start(stmt_type(&lowered.stmt), start.elapsed());
        self.reset_warnings(&lowered.stmt);
        // Go's `GetPlanFromPlanCache` warns `skip prepared plan-cache: <reason>` on every
        // EXECUTE of a statement prepared as uncacheable (#512).
        self.warnings.extend(plan_cache::execute_warnings(&reason));
        // No plan cache: every execute plans again (Go counts a prepared plan cache miss).
        tidb_metrics::plan_cache_miss_prepared();
        self.timer.mark(Phase::Parse);
        self.run(&lowered, &params, ts).await
    }

    async fn use_db_inner(&mut self, db: &str) -> Result<(), SqlError> {
        if let Some(pm) = &self.privilege_manager
            && !pm.db_is_visible(&self.active_roles, db)
        {
            return Err(SqlError::db_access_denied(
                &self.auth_user,
                &self.auth_host,
                db,
            ));
        }
        let is = self.name_schema().await?;
        if is.schema_by_name(db).is_none() {
            return Err(SqlError::bad_db(db));
        }
        self.current_db = Some(db.to_owned());
        Ok(())
    }
}

/// The schema version the router compares with the new stack's
/// ([`router::PocEngine::admissible_schema_version`]): the synced schema's, only while the
/// schema sync admits statements on the new stack ([`poc_domain::Registry::new_stack_schema_version`]);
/// `None` without schema sync. Never the ADR 0003 cache's newest entry: that one is not
/// covered by the schema lease or the node's registration.
// glue: VS1 plan §4.1 (the PoC's side of the router's InfoSchema version check)
pub(crate) fn router_schema_version(
    registry: Option<&poc_domain::Registry>,
    now: Instant,
) -> Option<i64> {
    registry?.new_stack_schema_version(now).ok()
}

impl router::PocEngine for Session {
    fn admissible_schema_version(&self) -> Option<i64> {
        router_schema_version(
            self.shared.sync.as_ref().map(|s| &**s.registry()),
            Instant::now(),
        )
    }

    fn in_txn(&self) -> bool {
        // BEGIN / START TRANSACTION and autocommit=0 are rejected: never in a transaction.
        false
    }

    fn values(&self) -> PocValues {
        self.poc_values()
    }

    fn begin_command(&mut self) {
        self.timer.begin();
        self.mdl = None;
        self.stmt.stmt_type = None;
    }

    fn start_statement(&mut self, kind: StmtType, parse: Duration) {
        self.stmt.start(kind, parse);
        self.timer.mark(Phase::Parse);
    }

    fn run<'a>(
        &'a mut self,
        lowered: &'a Lowered,
    ) -> router::BoxFut<'a, Result<Response, SqlError>> {
        Box::pin(self.run_query(lowered))
    }

    fn prepare(
        &mut self,
        stmt_id: u32,
        lowered: Lowered,
    ) -> router::BoxFut<'_, Result<PreparedInfo, SqlError>> {
        Box::pin(self.prepare_lowered(stmt_id, lowered))
    }

    fn execute(
        &mut self,
        stmt_id: u32,
        params: Vec<Param>,
    ) -> router::BoxFut<'_, Result<Response, SqlError>> {
        Box::pin(self.execute_inner(stmt_id, params))
    }

    fn close(&mut self, stmt_id: u32) {
        self.stmts.remove(&stmt_id);
    }

    fn clear_warnings(&mut self) {
        self.warnings.clear();
    }

    fn set_cacheability(&mut self, stmt_id: u32, c: plan_cache::PrepareCacheability) {
        if let Some(s) = self.stmts.get_mut(&stmt_id) {
            s.plan_cache_reason = c.reason;
        }
        self.warnings.extend(c.warnings);
    }

    fn set_prev_stmt(&mut self, last: &router::LastStmtInfo) {
        self.prev_stmt = last.clone();
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
    let rows = poc_executor::execute(&plan, &mut snapshot, &cx).await?;
    Ok(rows.iter().any(|r| {
        r.first() == Some(&Value::from("new_collation_enabled"))
            && matches!(r.get(1), Some(Value::Bytes(b)) if b.eq_ignore_ascii_case(b"true"))
    }))
}

#[cfg(test)]
mod fence_tests {
    use tidb_sql::ir::SetAssignment;

    use super::*;

    const NOT_IN_TXN: &str = ": explicit transactions are not supported by the read-only node \
                              (every statement runs in autocommit mode)'";

    /// The explicit-transaction fence answers 1235 with the fixed text, never panics.
    #[test]
    fn explicit_transactions_answer_1235() {
        for (sql, what) in [
            ("BEGIN", "BEGIN/START TRANSACTION"),
            ("START TRANSACTION", "BEGIN/START TRANSACTION"),
            ("COMMIT", "COMMIT"),
            ("ROLLBACK", "ROLLBACK"),
        ] {
            let lowered = tidb_sql::parse_and_lower(sql).unwrap();
            let e = txn_fence(&lowered.stmt).unwrap_or_else(|| panic!("{sql}: no fence"));
            assert_eq!(e.code, 1235, "{sql}");
            assert_eq!(
                e.message,
                format!("This version of TiDB doesn't yet support '{what}{NOT_IN_TXN}"),
                "{sql}"
            );
        }
        let select = tidb_sql::parse_and_lower("SELECT 1").unwrap();
        assert!(txn_fence(&select.stmt).is_none());
    }

    #[test]
    fn autocommit_off_answers_1235() {
        let value = |sql: &str| match tidb_sql::parse_and_lower(sql).unwrap().stmt {
            Statement::Set(a) => match a.into_iter().next() {
                Some(SetAssignment::SysVar { value, .. }) => value,
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        };
        for sql in [
            "SET autocommit = 0",
            "SET autocommit = OFF",
            "SET @@autocommit = 'false'",
        ] {
            let e = autocommit_fence(&value(sql)).unwrap_err();
            assert_eq!(e.code, 1235, "{sql}");
            assert_eq!(
                e.message,
                format!("This version of TiDB doesn't yet support 'autocommit=0{NOT_IN_TXN}")
            );
        }
        assert!(autocommit_fence(&value("SET autocommit = 1")).is_ok());
        assert_eq!(
            autocommit_fence(&value("SET autocommit = 'x'"))
                .unwrap_err()
                .code,
            1231
        );
    }
}
