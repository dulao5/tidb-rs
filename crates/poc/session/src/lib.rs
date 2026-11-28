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
mod show;
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
    /// Shared snapshot of `mysql.tidb` (`tidb_gc_*`), updated alongside `new_globals`.
    tidb_table: Option<Arc<parking_lot::RwLock<HashMap<String, String>>>>,
}

impl Factory {
    pub fn new(shared: Shared) -> Factory {
        Factory {
            shared: Arc::new(shared),
            new_stack: NewStackMode::Off,
            new_store: None,
            new_globals: None,
            tidb_table: None,
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

    /// Attach the synced `mysql.tidb` snapshot (`tidb_gc_*`) for both stacks.
    pub fn with_tidb_table(
        mut self,
        tidb_table: Arc<parking_lot::RwLock<HashMap<String, String>>>,
    ) -> Factory {
        self.tidb_table = Some(tidb_table);
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
            // Go's `session.SetCollation` at the handshake (`SetSystemVarWithoutValidation`).
            session_values: router::sync::SET_NAMES_VARIABLES
                .iter()
                .map(|v| ((*v).to_owned(), Some(charset.to_owned())))
                .chain([(
                    "collation_connection".to_owned(),
                    Some(collation.to_owned()),
                )])
                .collect(),
            globals: self.new_globals.clone(),
            tidb_table: self.tidb_table.clone(),
            default_vars_loaded: false,
            timer: Timer::default(),
            mdl: None,
            stmt: StmtMetrics::default(),
            router: None,
            prev_stmt: router::LastStmtInfo::default(),
            set_applied: 0,
        };
        if self.new_stack != NewStackMode::Off {
            let domain = self
                .shared
                .sync
                .as_ref()
                .and_then(|s| s.ported_domain().cloned());
            let mut new = Box::new(router::newstack::NewSession::with_globals(
                domain,
                self.new_store.clone(),
                self.shared.kv.exec_pool(),
                // Without a loader the cache is never built: the global values are unknown,
                // so the router keeps the session on the PoC.
                self.new_globals
                    .clone()
                    .unwrap_or_else(|| Arc::new(tidb_systable::sysvar_cache::SysVarCache::new())),
                self.tidb_table.clone(),
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
    /// The prototype's session variables, over the cluster's global values (Go's default
    /// variables until the sysvar cache is built): with `--new-stack=off` (no router, no new
    /// session), the input of a PREPARE's plan-cache decision ([`plan_cache`]); in every
    /// mode, the session the prototype's `SET` validates its values against and writes them
    /// into, and the one `@@name` reads ([`constant`]). Made at the first statement that
    /// needs it.
    default_vars: Option<Box<tidb_session::Session>>,
    /// The session values the prototype gave its session variables (the handshake's charset,
    /// the `SET`s it accepted), in order, last write per variable: `default_session` writes
    /// them again (Go's `SetSystemVarWithoutValidation`) when it makes `default_vars` anew
    /// once the cluster's global values are loaded. `None`: the variable's global value
    /// (`SET x = DEFAULT`, Go's `getVarValue`), read again from the cluster's globals then.
    session_values: Vec<SessionValue>,
    /// The sysvar cache `default_vars` takes the cluster's global values from (the
    /// factory's; D06.5 spec §8).
    globals: Option<Arc<tidb_systable::sysvar_cache::SysVarCache>>,
    /// Shared snapshot of `mysql.tidb` (`tidb_gc_*`), updated alongside `globals`.
    tidb_table: Option<Arc<parking_lot::RwLock<HashMap<String, String>>>>,
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
    /// How many assignments of the current statement's `SET` were applied (`run_set`); 0 for
    /// any other statement.
    set_applied: usize,
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
    show_warnings: bool,
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
        Statement::ShowWarnings { .. } | Statement::ShowVariables { .. } => StmtType::Show,
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

/// `SET autocommit = value`, after Go's validation turned the value into `ON` / `OFF`
/// (`check_set`): on is accepted, off is the explicit-transaction fence (1235).
fn autocommit_fence(normalized: &str) -> Result<(), SqlError> {
    if normalized == tidb_sessionctx::variable::Off {
        return Err(not_in_txn("autocommit=0"));
    }
    Ok(())
}

/// Go's `SetExecutor.setCharset(cs, co, isSetName)` (`set.go:257`) for `SET NAMES` and
/// `SET CHARACTER SET` on the prototype: `cs` is the charset the parser already checked
/// (`CharsetName`: `SET NAMES nosuch` is the parser's 1115) or `None` for DEFAULT
/// (`mysql.DefaultCharset`), `co` the `COLLATE` name. Without a collation, utf8mb4 takes the
/// session's `default_collation_for_utf8mb4` and any other charset its default collation.
/// A collation is looked up as Go's `collate.GetCollationByName` does (1273 for an unknown
/// one) and must belong to `cs` (`ErrCollationCharsetMismatch`, 1253). Returns the charset,
/// the collation and the variables Go then writes with `SessionVars.SetSystemVar`.
// glue: SetExecutor.setCharset (D12.12) on the prototype's SET NAMES / SET CHARACTER SET
fn set_charset(
    vars: &mut tidb_sessionctx::variable::session::SessionVars,
    cs: Option<&str>,
    co: Option<&str>,
    is_set_name: bool,
) -> Result<(String, String, Vec<VarWrite>), SqlError> {
    let err = |e: tidb_base::Error| router::sql_error(&e);
    let cs = cs.unwrap_or(tidb_base::mysql::DefaultCharset).to_owned();
    let co = match co.filter(|c| !c.is_empty()) {
        None if cs == tidb_base::mysql::UTF8MB4Charset => {
            vars.default_collation_for_utf8mb4.clone()
        }
        None => tidb_parser::gocharset::get_default_collation(&cs).map_err(err)?,
        Some(co) => {
            let coll = tidb_types::collate::get_collation_by_name(co).map_err(err)?;
            if coll.charset_name != cs {
                return Err(err(tidb_parser::gocharset::ErrCollationCharsetMismatch
                    .gen_with_stack_by_args(tidb_base::go_args![
                        coll.name.clone(),
                        cs
                    ])));
            }
            coll.name.clone()
        }
    };
    let mut writes = Vec::new();
    if is_set_name {
        for v in router::sync::SET_NAMES_VARIABLES {
            writes.push(VarWrite::Checked(v, cs.clone()));
        }
        writes.push(VarWrite::Checked("collation_connection", co.clone()));
        return Ok((cs, co, writes));
    }
    // Set charset statement, see also https://dev.mysql.com/doc/refman/8.0/en/set-character-set.html.
    for v in router::sync::SET_CHARSET_VARIABLES {
        writes.push(VarWrite::Checked(v, cs.clone()));
    }
    let cs_db = vars
        .get_global_system_var("character_set_database")
        .map_err(err)?;
    let co_db = vars
        .get_global_system_var("collation_database")
        .map_err(err)?;
    writes.push(VarWrite::Checked("character_set_connection", cs_db));
    writes.push(VarWrite::Checked("collation_connection", co_db));
    Ok((cs, co, writes))
}

/// The charsets the prototype converts results to (`SET NAMES`, `SET CHARACTER SET`): any
/// other charset Go knows is not supported.
fn prototype_charset(cs: &str) -> bool {
    matches!(cs, "utf8mb4" | "utf8" | "binary" | "latin1" | "ascii")
}

/// Session variables the prototype accepts in a session `SET` and keeps in its session
/// variables (so `@@name` reads them back) without anything else to do: they are provably
/// result-neutral for anything this node executes on the PoC path, and the router syncs them
/// into the new session ([`router::sync::SYNCED_SYSVARS`]), which applies them with Go's
/// semantics.
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
    // Connect-time SETs of drivers and pools (#505). Only DML/DDL or unported functions read these.
    "sql_safe_updates",
    "foreign_key_checks",
    "unique_checks",
    "innodb_lock_wait_timeout",
    "group_concat_max_len",
    // Safe/validated switches explicitly allowed on PoC and synced into the new session (#541).
    "div_precision_increment",
    "tidb_enable_noop_functions",
    "tidb_skip_isolation_level_check",
    "tidb_enable_prepared_plan_cache",
];

/// `sql_select_limit`'s default (`math.MaxUint64`): no limit, the only value the prototype
/// accepts (Go's `buildSelect` adds a Limit for any other value; the prototype has none).
const NO_SELECT_LIMIT: &str = "18446744073709551615";

impl Session {
    /// Syncs the prototype's live connection fields (`connection_id`, `current_db`,
    /// `prev_stmt`) onto `default_session`'s `SessionVars` before reading system variables.
    fn sync_poc_context_to_default_vars(&mut self) -> Result<(), SqlError> {
        let connection_id = self.connection_id;
        let current_db = self.current_db.clone().unwrap_or_default();
        let prev_stmt = self.prev_stmt.clone();
        let vars = self.default_session()?.vars_mut();
        vars.connection_id = connection_id;
        vars.current_db = current_db;
        prev_stmt.apply_to(vars);
        Ok(())
    }

    /// A SELECT without FROM ([`constant`]), over the prototype's session variables; the
    /// warnings it appends go to the statement's.
    fn const_select(&mut self, sel: &Select, params: &[Literal]) -> Result<ResultSet, SqlError> {
        self.sync_poc_context_to_default_vars()?;
        let vars = self
            .default_vars
            .as_deref_mut()
            .expect("made by default_session")
            .vars_mut();
        let mut env = ConstEnv {
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
            vars,
            warnings: &mut self.warnings,
        };
        env.select(sel, params)
    }

    /// `SHOW [GLOBAL|SESSION] VARIABLES [LIKE ... | WHERE ...]` over live `SessionVars` and
    /// cluster globals.
    fn show_variables_stmt(
        &mut self,
        global: bool,
        like: Option<&tidb_sql::ir::ShowExpr>,
        where_: Option<&tidb_sql::ir::ShowExpr>,
        params: &[Literal],
    ) -> Result<ResultSet, SqlError> {
        self.sync_poc_context_to_default_vars()?;
        let vars = self
            .default_vars
            .as_deref_mut()
            .expect("made by default_session")
            .vars_mut();
        let have_ssl = if self.shared.tls_enabled {
            "YES"
        } else {
            "DISABLED"
        };
        let mut rs = show::show_variables(
            vars,
            global,
            like,
            where_,
            params,
            &self.shared.server_version,
            have_ssl,
            &mut self.warnings,
        )?;
        for c in &mut rs.columns {
            apply_result_charset(c, self.results_charset.as_deref());
        }
        Ok(rs)
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
        self.stmt.show_warnings = matches!(lowered.stmt, Statement::ShowWarnings { .. });
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
        self.set_applied = 0;
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
            Statement::ShowVariables {
                global,
                like,
                where_,
            } => self
                .show_variables_stmt(*global, like.as_ref(), where_.as_ref(), params)
                .map(Response::ResultSet),
        }
    }

    async fn select(
        &mut self,
        sel: &Select,
        params: &[Literal],
        ts: Option<TsFuture>,
    ) -> Result<ResultSet, SqlError> {
        if sel.from.is_none() {
            let mut rs = self.const_select(sel, params)?;
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
        self.default_session()?;
        let vars = self
            .default_vars
            .as_mut()
            .expect("made by default_session")
            .vars_mut();
        let (charset, collation, results_charset, warnings, values) = (
            &mut self.charset,
            &mut self.collation,
            &mut self.results_charset,
            &mut self.warnings,
            &mut self.session_values,
        );
        let (applied, result) = run_set(vars, assignments, |change| {
            if let Some((cs, coll)) = change.names {
                *charset = cs;
                *collation = coll;
            }
            if let Some(r) = change.results {
                *results_charset = r;
            }
            warnings.extend(change.warnings);
            record_session_values(values, change.values, change.from_default);
        });
        self.set_applied = applied;
        result
    }
}

/// A session value of the prototype's session variables: the variable, and its value
/// (`None`: the variable's global value, `SET x = DEFAULT`).
type SessionValue = (String, Option<String>);

/// Records the values a `SET` assignment left in the prototype's session variables, for
/// `Session::default_session` to write them again when it makes the session anew; a
/// `SET x = DEFAULT` records the global value, read again from the cluster's globals then.
fn record_session_values(
    values: &mut Vec<SessionValue>,
    written: Vec<(String, String)>,
    from_default: bool,
) {
    for (name, value) in written {
        if let Some(sv) = tidb_sessionctx::variable::get_sys_var(&name) {
            values.retain(|(n, _)| n != sv.name && !sv.aliases.contains(&n.as_str()));
        } else {
            values.retain(|(n, _)| *n != name);
        }
        values.push((name, (!from_default).then_some(value)));
    }
}

/// The prototype's session variables over `globals`, with its session values written over
/// them (Go's `SetSystemVarWithoutValidation`); a `DEFAULT` value is the global value of
/// `globals` (Go's `getVarValue`).
fn build_default_session(
    globals: router::newstack::ClusterGlobals,
    session_values: &[SessionValue],
) -> Result<tidb_session::Session, SqlError> {
    let err = |e: tidb_base::Error| router::sql_error(&e);
    let mut s = tidb_session::Session::new(tidb_session::Dialect::MySQL, Box::new(globals));
    s.load_common_global_variables_if_needed().map_err(err)?;
    for (name, value) in session_values {
        let value = match value {
            Some(v) => v.clone(),
            None => s.vars_mut().get_global_system_var(name).map_err(err)?,
        };
        s.vars_mut()
            .set_system_var_without_validation(name, &value)
            .map_err(err)?;
    }
    Ok(s)
}

/// A write of the prototype's session variables that a `SET` assignment makes.
#[derive(Debug, PartialEq)]
enum VarWrite {
    /// A value `SysVar.Validate` already normalized: Go's `SetSessionFromHook`.
    Validated(String, String),
    /// Go's `SessionVars.SetSystemVar` (validate, then store): `setCharset`'s writes.
    Checked(&'static str, String),
}

/// What a `SET` assignment the prototype accepted changes in its session.
#[derive(Debug, Default, PartialEq)]
struct SetChange {
    /// `SET NAMES`: the charset and collation.
    names: Option<(String, String)>,
    /// `@@character_set_results` (`Some(None)`: NULL).
    results: Option<Option<String>>,
    /// The writes of the session variables, made by [`run_set`].
    writes: Vec<VarWrite>,
    /// The session variables the writes changed and the values they hold after them (set
    /// by [`run_set`]).
    values: Vec<(String, String)>,
    /// The warnings the assignment appended (set by [`run_set`]): a validation's, Go's
    /// `ErrSettingNoopVariable`.
    warnings: Vec<SQLWarn>,
    /// `SET x = DEFAULT`: the session value is the global value (Go's `getVarValue`).
    from_default: bool,
}

/// `SET assignments` on the prototype as Go's `SetExecutor.Next` runs them (`set.go:55-110`):
/// each assignment is checked ([`check_set`]), written into `vars` (the prototype's session
/// variables) and applied (`apply`) before the next one is checked; the first error ends the
/// statement, and the assignments before it stay applied. The warnings an assignment
/// appends to `vars`'s statement context are moved into its change, as Go keeps them in the
/// statement's `StmtCtx`. Returns how many assignments were applied, and the statement's
/// result.
// glue: the assignment loop of SetExecutor.Next (D12.12) on the prototype's SET
fn run_set(
    vars: &mut tidb_sessionctx::variable::session::SessionVars,
    assignments: &[SetAssignment],
    mut apply: impl FnMut(SetChange),
) -> (usize, Result<(), SqlError>) {
    for (i, a) in assignments.iter().enumerate() {
        let start = i64::from(vars.stmt_ctx().warning_count());
        let result = check_set(vars, a).and_then(|change| write_vars(vars, change));
        let warnings = vars.stmt_ctx().truncate_warnings(start);
        match result {
            Ok(mut change) => {
                change.warnings = warnings;
                apply(change);
            }
            Err(e) => return (i, Err(e)),
        }
    }
    (assignments.len(), Ok(()))
}

/// Makes the writes of `change` in `vars` and records the values they leave.
fn write_vars(
    vars: &mut tidb_sessionctx::variable::session::SessionVars,
    mut change: SetChange,
) -> Result<SetChange, SqlError> {
    for w in &change.writes {
        let name = match w {
            VarWrite::Validated(name, value) => {
                let sv = tidb_sessionctx::variable::get_sys_var(name)
                    .expect("validated before the write");
                sv.set_session_from_hook(vars, value)
                    .map_err(|e| router::sql_error(&e))?;
                name.as_str()
            }
            VarWrite::Checked(name, value) => {
                vars.set_system_var(name, value)
                    .map_err(|e| router::sql_error(&e))?;
                name
            }
        };
        change
            .values
            .push((name.to_owned(), vars.get_system_var(name).0));
    }
    Ok(change)
}

/// The checks of one `SET` assignment on the prototype, in Go's order, before anything is
/// written: the change to make, or the assignment's error. `vars` is the prototype's session
/// the values are validated against (`sys_var.validate`).
fn check_set(
    vars: &mut tidb_sessionctx::variable::session::SessionVars,
    a: &SetAssignment,
) -> Result<SetChange, SqlError> {
    match a {
        SetAssignment::Names { charset, collation } => {
            let (cs, co, writes) =
                set_charset(vars, charset.as_deref(), collation.as_deref(), true)?;
            if !prototype_charset(&cs) {
                return Err(SqlError::not_supported(format!("SET NAMES {cs}")));
            }
            Ok(SetChange {
                names: Some((cs.clone(), co)),
                results: Some(Some(cs)),
                writes,
                ..SetChange::default()
            })
        }
        SetAssignment::Charset { charset } => {
            let (cs, _, writes) = set_charset(vars, charset.as_deref(), None, false)?;
            if !prototype_charset(&cs) {
                return Err(SqlError::not_supported(format!("SET CHARACTER SET {cs}")));
            }
            Ok(SetChange {
                results: Some(Some(cs)),
                writes,
                ..SetChange::default()
            })
        }
        // Not supported until the misc functions (D08.9) port getvar / setvar: Go stores the
        // value with its expression's type, which a later `@x` reads (maintainer decision on
        // #505: no hand-written types). A known difference of the prototype (ADR 0004).
        SetAssignment::UserVar { .. } => Err(SqlError::not_supported("user variables")),
        SetAssignment::SysVar {
            name,
            global,
            value,
        } => {
            // Go's `setSysVariable`: an unknown name is 1193; a removed one is parsed and
            // ignored (also with GLOBAL: the name is checked first).
            let Some(sys_var) = tidb_sessionctx::variable::get_sys_var(name) else {
                if tidb_sessionctx::variable::removed::is_removed_sys_var(name) {
                    return Ok(SetChange::default());
                }
                return Err(router::sql_error(
                    &tidb_sessionctx::variable::error::ErrUnknownSystemVar
                        .gen_with_stack_by_args(tidb_base::go_args![name.to_owned()]),
                ));
            };
            let mut is_global = *global;
            let mut legacy_instance_warning = false;
            if sys_var.has_instance_scope() && !is_global && vars.enable_legacy_instance_scope {
                is_global = true;
                legacy_instance_warning = true;
            }
            let scope = if is_global {
                tidb_sessionctx::variable::ScopeGlobal
            } else {
                tidb_sessionctx::variable::ScopeSession
            };
            // Go `SetExecutor.setSysVariable` evaluates `getVarValue` (`set.go:151,188`) before
            // `SetGlobalSysVar` / `SetSystemVar` validates scope, type, and value hooks.
            let text = if is_global || (*value == SetValue::Default && sys_var.has_none_scope()) {
                match value {
                    SetValue::Default => Some(sys_var.value.clone()),
                    _ => router::sync::value_text(vars, sys_var.name, value),
                }
            } else if *value == SetValue::Default {
                match router::sync::value_text(vars, sys_var.name, value) {
                    Some(v) => Some(v),
                    None => {
                        sys_var
                            .validate_scope(scope)
                            .map_err(|e| router::sql_error(&e))?;
                        None
                    }
                }
            } else {
                router::sync::value_text(vars, sys_var.name, value)
            };
            let Some(text) = text else {
                // Non-literal expressions on `SET` cannot yet be evaluated by the prototype.
                return Err(SqlError::not_supported("this SET value"));
            };
            let (normalized, validated) = sys_var.validate(vars, &text, scope);
            validated.map_err(|e| router::sql_error(&e))?;
            if is_global {
                if legacy_instance_warning {
                    vars.stmt_ctx().append_warning(
                        tidb_base::dbterror::exeerrors::ErrInstanceScope
                            .fast_gen_by_args(tidb_base::go_args![sys_var.name.to_owned()]),
                    );
                }
                return Err(SqlError::not_supported("SET GLOBAL"));
            }
            if sys_var.is_noop
                && !tidb_sessionctx::variable::tidb_vars::EnableNoopVariables
                    .load(std::sync::atomic::Ordering::SeqCst)
            {
                // The variable is a noop. For compatibility we allow it to still
                // be changed, but we append a warning since users might be expecting
                // something that's not going to happen.
                vars.stmt_ctx().append_warning(
                    tidb_base::dbterror::exeerrors::ErrSettingNoopVariable
                        .fast_gen_by_args(tidb_base::go_args![sys_var.name.to_owned()]),
                );
            }
            let mut change = SetChange::default();
            match sys_var.name {
                "autocommit" => autocommit_fence(&normalized)?,
                // NULL is the empty value (`getVarValue`): columns keep their charset.
                "character_set_results" => {
                    change.results = Some((!normalized.is_empty()).then(|| normalized.clone()));
                }
                "character_set_client"
                | "character_set_connection"
                | "collation_connection"
                | "tidb_enable_window_function" => {}
                // The validated value is upper case (`FormatSQLModeStr`).
                "sql_mode" if normalized.contains("PAD_CHAR_TO_FULL_LENGTH") => {
                    return Err(SqlError::not_supported(
                        "this sql_mode (PAD_CHAR_TO_FULL_LENGTH changes CHAR results)",
                    ));
                }
                "sql_mode" => {}
                "sql_select_limit" if normalized != NO_SELECT_LIMIT => {
                    return Err(SqlError::not_supported(
                        "sql_select_limit other than 18446744073709551615 (the prototype \
                         applies no select limit)",
                    ));
                }
                "sql_select_limit" => {}
                n if IGNORED_SYSVARS.contains(&n) => {}
                n => {
                    return Err(SqlError::not_supported(format!(
                        "setting system variable '{n}'"
                    )));
                }
            }
            change.writes = vec![VarWrite::Validated(sys_var.name.to_owned(), normalized)];
            change.from_default = *value == SetValue::Default;
            Ok(change)
        }
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
                let r = self.query_inner(sql).await;
                self.prev_stmt = match &r {
                    Err(e)
                        if self.stmt.stmt_type.is_none()
                            && matches!(e.code, 1064 | 1115 | 1273) =>
                    {
                        router::LastStmtInfo::from_parse_error(&self.prev_stmt, true)
                    }
                    _ if self.stmt.show_warnings => {
                        router::LastStmtInfo::from_poc_show_warnings(&self.prev_stmt, &r)
                    }
                    _ => {
                        let is_select = self.stmt.stmt_type == Some(StmtType::Select);
                        router::LastStmtInfo::from_poc_stmt(&self.prev_stmt, &r, is_select)
                    }
                };
                r
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
        self.stmt.show_warnings = false;
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
                let r = self.execute_inner(stmt_id, params).await;
                self.prev_stmt = if self.stmt.show_warnings {
                    router::LastStmtInfo::from_poc_show_warnings(&self.prev_stmt, &r)
                } else {
                    let is_select = self.stmt.stmt_type == Some(StmtType::Select);
                    router::LastStmtInfo::from_poc_stmt(&self.prev_stmt, &r, is_select)
                };
                r
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
    fn parse_options(&mut self) -> Result<tidb_parser::ParseOptions, SqlError> {
        let vars = self.default_vars()?;
        Ok(tidb_parser::ParseOptions {
            window_functions: vars.enable_window_function,
            sql_mode: tidb_base::mysql::SQLMode(vars.sql_mode),
        })
    }

    async fn query_inner(&mut self, sql: &str) -> Result<Response, SqlError> {
        self.timer.begin();
        self.mdl = None;
        self.stmt.stmt_type = None;
        self.stmt.show_warnings = false;
        let opts = self.parse_options()?;
        let start = Instant::now();
        let lowered = match tidb_sql::parse_and_lower_with(sql, &opts) {
            Ok(l) => l,
            Err(e) => {
                if matches!(e.code, 1064 | 1115 | 1273) {
                    // Go fails before `ResetContextOfStmt`: the warnings stay and the parse error
                    // is appended to the previous statement context.
                    let wrapped = router::log_and_wrap_parse_error(
                        self.connection_id,
                        "",
                        tidb_base::errors::redact_log_enabled_load(),
                        sql,
                        e,
                    );
                    self.warnings.push(SQLWarn {
                        level: tidb_base::contextutil::WarnLevelError.into(),
                        err: router::parse_error_warn(&wrapped),
                    });
                    return Err(wrapped);
                }
                // A statement the prototype does not lower: Go would have started it.
                self.warnings.clear();
                return Err(e);
            }
        };
        let parse = start.elapsed();
        tidb_metrics::parse_duration(parse);
        self.stmt.start(stmt_type(&lowered.stmt), parse);
        self.stmt.show_warnings = matches!(lowered.stmt, Statement::ShowWarnings { .. });
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
        let opts = self.parse_options()?;
        let stmt = tidb_sql::ir::parse_one_with(sql, &opts)?;
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
        Ok(self.default_session()?.vars())
    }

    /// The session behind [`Session::default_vars`], also the one the prototype's `SET`
    /// validates its values against and writes them into ([`run_set`]): the cluster's global
    /// values once the sysvar cache is built, the compiled defaults before, with the
    /// session's own values ([`Session::session_values`]) written over them.
    fn default_session(&mut self) -> Result<&mut tidb_session::Session, SqlError> {
        let built = self.globals.as_ref().is_some_and(|c| !c.needs_rebuild());
        if built && !self.default_vars_loaded {
            self.default_vars = None;
        }
        if self.default_vars.is_none() {
            self.default_vars_loaded = built;
            let globals = match &self.globals {
                Some(cache) if built => router::newstack::ClusterGlobals::new(Arc::clone(cache))
                    .with_tidb_table_opt(self.tidb_table.clone()),
                _ => {
                    static WARNED: std::sync::Once = std::sync::Once::new();
                    WARNED.call_once(|| {
                        tidb_base::log_warn!(
                            "the global variables of the cluster are not loaded yet (mysql.global_variables); the prototype uses the compiled defaults until they are"
                        );
                    });
                    router::newstack::ClusterGlobals::compiled_defaults()
                        .with_tidb_table_opt(self.tidb_table.clone())
                }
            };
            let s = build_default_session(globals, &self.session_values)?;
            self.default_vars = Some(Box::new(s));
        }
        Ok(self.default_vars.as_deref_mut().expect("set above"))
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
        match &lowered.stmt {
            Statement::Select(sel) if sel.from.is_some() => {
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
            }
            Statement::Select(sel) if param_count == 0 => {
                columns = self.const_select(sel, &[])?.columns;
                for c in &mut columns {
                    apply_result_charset(c, self.results_charset.as_deref());
                }
            }
            Statement::ShowVariables { like, where_, .. } => {
                show::validate_show_filters(like.as_ref(), where_.as_ref())?;
                columns = show::show_variables_columns().to_vec();
                for c in &mut columns {
                    apply_result_charset(c, self.results_charset.as_deref());
                }
            }
            _ => {}
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
        self.stmt.show_warnings = false;
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

    fn set_applied(&self) -> usize {
        self.set_applied
    }

    fn begin_command(&mut self) {
        self.timer.begin();
        self.mdl = None;
        self.stmt.stmt_type = None;
        self.stmt.show_warnings = false;
        self.stmt.show_warnings = false;
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

    fn append_error_warning(&mut self, warn: SQLWarn) {
        self.warnings.push(warn);
    }
}

/// The rows of `mysql.tidb` (`VARIABLE_NAME`, `VARIABLE_VALUE`), read once at server start:
/// what Go's `BootstrapSession` reads from it, the system time zone and the collation
/// framework.
pub struct TidbTable {
    rows: Vec<Vec<Value>>,
}

impl TidbTable {
    /// Reads `mysql.tidb` at a fresh TSO.
    pub async fn read(kv: &KvClient, schema: &SchemaCache) -> Result<TidbTable, SqlError> {
        Ok(TidbTable {
            rows: tidb_table_rows(kv, schema).await?,
        })
    }

    fn value(&self, name: &str) -> Option<Option<&[u8]>> {
        let row = self
            .rows
            .iter()
            .find(|r| r.first() == Some(&Value::from(name)))?;
        Some(match row.get(1) {
            Some(Value::Bytes(b)) => Some(b.as_slice()),
            _ => None,
        })
    }

    /// `new_collation_enabled` (set once at cluster bootstrap; Go: `loadCollationParameter`).
    /// Missing row: old framework.
    pub fn new_collation_enabled(&self) -> bool {
        matches!(self.value("new_collation_enabled"), Some(Some(b)) if b.eq_ignore_ascii_case(b"true"))
    }

    /// Loads the cluster's system time zone: `system_tz`, written at bootstrap from
    /// `InferSystemTZ`. Go's `BootstrapSession` reads it with `getTableValue` (session.go:3692),
    /// fails when the row is missing (`errResultIsEmpty`) and hands it to
    /// `timeutil.SetSystemTZ`; the server start then copies it into `@@system_time_zone`
    /// (`setSystemTimeZoneVariable`, server.go:1133), which keeps its default "CST" when the
    /// zone is "" or "System".
    // glue: BootstrapSession's getTableValue(mysql.tidb, system_tz) on the prototype's mysql.tidb read, then the server start
    pub fn load_system_tz(&self) -> Result<(), SqlError> {
        let value = self
            .value("system_tz")
            // Go's `errResultIsEmpty`.
            .ok_or_else(|| {
                let e = tidb_base::mysql::new_err(tidb_base::errno::ErrResultIsEmpty, vec![]);
                SqlError::new(e.code, String::from_utf8_lossy(&e.message).into_owned())
            })?;
        let tz = String::from_utf8_lossy(value.unwrap_or_default()).into_owned();
        tidb_base::timeutil::set_system_tz(&tz);
        tidb_server::server::set_system_time_zone_variable();
        Ok(())
    }
}

/// Reads `new_collation_enabled` from `mysql.tidb` ([`TidbTable::new_collation_enabled`]).
pub async fn detect_new_collation(kv: &KvClient, schema: &SchemaCache) -> Result<bool, SqlError> {
    Ok(TidbTable::read(kv, schema).await?.new_collation_enabled())
}

/// The rows `(VARIABLE_NAME, VARIABLE_VALUE)` of `mysql.tidb` at a fresh TSO.
async fn tidb_table_rows(kv: &KvClient, schema: &SchemaCache) -> Result<Vec<Vec<Value>>, SqlError> {
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
    poc_executor::execute(&plan, &mut snapshot, &cx).await
}

#[cfg(test)]
mod fence_tests {

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

    /// The session the prototype's `SET` validates against, as `Session::default_session`
    /// makes it: over a sysvar cache built from these rows of `mysql.global_variables` (the
    /// loader's rebuild), or over the compiled defaults when there are none.
    fn validation_session(rows: &[(&str, &str)]) -> tidb_session::Session {
        let mut s = tidb_session::Session::new(
            tidb_session::Dialect::MySQL,
            Box::new(cluster_globals(rows)),
        );
        s.load_common_global_variables_if_needed().unwrap();
        s
    }

    /// The cluster's global values: a sysvar cache built from these rows of
    /// `mysql.global_variables`, or the compiled defaults when there are none.
    fn cluster_globals(rows: &[(&str, &str)]) -> router::newstack::ClusterGlobals {
        if rows.is_empty() {
            router::newstack::ClusterGlobals::compiled_defaults()
        } else {
            let table: HashMap<String, String> = rows
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect();
            let cache = tidb_systable::sysvar_cache::SysVarCache::new();
            cache.rebuild(None, &mut || Ok(table.clone())).unwrap();
            router::newstack::ClusterGlobals::new(Arc::new(cache))
        }
    }

    /// `SET` on the prototype as `Session::set` runs it, validated against `session`: the
    /// change of each applied assignment, in order, and the statement's result.
    fn run_over(
        session: &mut tidb_session::Session,
        sql: &str,
    ) -> (Vec<SetChange>, Result<(), SqlError>) {
        let Statement::Set(a) = tidb_sql::parse_and_lower(sql).unwrap().stmt else {
            panic!("{sql}");
        };
        let mut changes = Vec::new();
        let (applied, result) = run_set(session.vars_mut(), &a, |c| changes.push(c));
        assert_eq!(applied, changes.len(), "{sql}");
        (changes, result)
    }

    /// `run_over` with the compiled defaults (the cache not built yet).
    fn run(sql: &str) -> (Vec<SetChange>, Result<(), SqlError>) {
        run_over(&mut validation_session(&[]), sql)
    }

    /// Validations that read another variable read the cluster's global value (the loader's
    /// sysvar cache), as Go's session does: with `tidb_skip_isolation_level_check` and
    /// `tidb_enable_noop_functions` ON on the cluster, Go accepts these SETs; with the
    /// defaults, both are rejected with Go's errors.
    #[test]
    fn validation_reads_the_cluster_global_values() {
        let isolation = "SET transaction_isolation = 'SERIALIZABLE'";
        let read_only = "SET transaction_read_only = 1";
        let whole = |r: Result<(), SqlError>| {
            let e = r.unwrap_err();
            (e.code, e.state.to_string(), e.message)
        };
        let mut defaults = validation_session(&[]);
        assert_eq!(
            whole(run_over(&mut defaults, isolation).1),
            (
                8048,
                "HY000".to_owned(),
                "The isolation level 'SERIALIZABLE' is not supported. Set tidb_skip_isolation_level_check=1 to skip this error".to_owned()
            )
        );
        assert_eq!(
            whole(run_over(&mut defaults, read_only).1),
            (
                1235,
                "42000".to_owned(),
                "function READ ONLY has only noop implementation in tidb now, use tidb_enable_noop_functions to enable these functions".to_owned()
            )
        );
        // The same with a built cache whose rows leave the switches at their defaults.
        let mut cluster_off = validation_session(&[("sql_mode", "")]);
        assert_eq!(
            run_over(&mut cluster_off, isolation).1.unwrap_err().code,
            8048
        );
        assert_eq!(
            run_over(&mut cluster_off, read_only).1.unwrap_err().code,
            1235
        );

        let mut skip_on = validation_session(&[("tidb_skip_isolation_level_check", "ON")]);
        let (changes, result) = run_over(&mut skip_on, isolation);
        result.unwrap();
        assert_eq!(names_and_results(&changes), vec![(None, None)]);
        assert_eq!(
            changes[0].values,
            [(
                "transaction_isolation".to_owned(),
                "SERIALIZABLE".to_owned()
            )]
        );
        // Go's validation appends its warning to the statement (`checkIsolationLevel`).
        assert_eq!(
            warnings(&changes),
            [(
                8048,
                "The isolation level 'SERIALIZABLE' is not supported. Set \
                 tidb_skip_isolation_level_check=1 to skip this error"
                    .to_owned()
            )]
        );
        assert!(
            run_over(
                &mut skip_on,
                "SET transaction_isolation = 'READ-UNCOMMITTED'"
            )
            .1
            .is_ok()
        );
        assert_eq!(run_over(&mut skip_on, read_only).1.unwrap_err().code, 1235);
        // The warnings are moved into the changes, none is left in the statement context.
        assert_eq!(skip_on.vars().stmt_ctx().warning_count(), 0);

        let mut noop_on = validation_session(&[("tidb_enable_noop_functions", "ON")]);
        let (changes, result) = run_over(&mut noop_on, read_only);
        result.unwrap();
        assert_eq!(names_and_results(&changes), vec![(None, None)]);
        assert_eq!(run_over(&mut noop_on, isolation).1.unwrap_err().code, 8048);
    }

    type NamesAndResults = (Option<(String, String)>, Option<Option<String>>);
    type NameValue = (String, String);

    /// What each change sets of the prototype's own connection state.
    fn names_and_results(changes: &[SetChange]) -> Vec<NamesAndResults> {
        changes
            .iter()
            .map(|c| (c.names.clone(), c.results.clone()))
            .collect()
    }

    /// The code and message of every warning the changes carry.
    fn warnings(changes: &[SetChange]) -> Vec<(u16, String)> {
        changes
            .iter()
            .flat_map(|c| &c.warnings)
            .map(|w| {
                let e = router::sql_error(&w.err);
                (e.code, e.message)
            })
            .collect()
    }

    /// A `SET` that succeeds: what it changes of the prototype's connection state; or its
    /// error.
    fn set(sql: &str) -> Result<NamesAndResults, SqlError> {
        let (changes, result) = run(sql);
        result?;
        let mut all: NamesAndResults = (None, None);
        for (names, results) in names_and_results(&changes) {
            all.0 = names.or(all.0);
            all.1 = results.or(all.1);
        }
        Ok(all)
    }

    /// A `SET` that succeeds: the session variables it changed, with their values after it,
    /// and its warnings.
    fn set_values(sql: &str) -> (Vec<NameValue>, Vec<(u16, String)>) {
        let (changes, result) = run(sql);
        result.unwrap_or_else(|e| panic!("{sql}: {e:?}"));
        let values = changes.iter().flat_map(|c| c.values.clone()).collect();
        (values, warnings(&changes))
    }

    /// The whole client error: code, SQLSTATE and message.
    fn err(sql: &str) -> (u16, String, String) {
        let e = set(sql).unwrap_err();
        (e.code, e.state.to_string(), e.message)
    }

    #[test]
    fn autocommit_off_answers_1235() {
        for sql in [
            "SET autocommit = 0",
            "SET autocommit = OFF",
            "SET @@autocommit = 'off'",
        ] {
            let e = set(sql).unwrap_err();
            assert_eq!(e.code, 1235, "{sql}");
            assert_eq!(
                e.message,
                format!("This version of TiDB doesn't yet support 'autocommit=0{NOT_IN_TXN}")
            );
        }
        for sql in [
            "SET autocommit = 1",
            "SET autocommit = ON",
            "SET autocommit = 'on'",
            "SET autocommit = TRUE",
            "SET autocommit = DEFAULT",
        ] {
            assert_eq!(set(sql), Ok((None, None)), "{sql}");
        }
    }

    /// A value Go's `SysVar.Validate` rejects is Go's error, with the value in the message
    /// (#489): `checkBoolSystemVar` accepts only ON / OFF / 0 / 1 for autocommit.
    #[test]
    fn autocommit_value_is_validated_as_go_does() {
        for (sql, value) in [
            ("SET autocommit = 'abc'", "abc"),
            ("SET @@session.autocommit = 'false'", "false"),
            ("SET autocommit = 2", "2"),
            ("SET autocommit = NULL", ""),
        ] {
            assert_eq!(
                err(sql),
                (
                    1231,
                    "42000".to_owned(),
                    format!("Variable 'autocommit' can't be set to the value of '{value}'")
                ),
                "{sql}"
            );
        }
        // A value the prototype cannot evaluate is not supported (it was 1231 without a value).
        assert_eq!(
            err("SET autocommit = 1 + 1"),
            (
                1235,
                "42000".to_owned(),
                "This version of TiDB doesn't yet support 'this SET value'".to_owned()
            )
        );
    }

    /// An unknown sql_mode is the parser's `ErrWrongValueForVar` (`mysql.NewErr`, not a TiDB
    /// error), which Go's `writeError` sends as `ErrUnknown` with its text (#489). The mode is
    /// named upper case (`FormatSQLModeStr` runs first).
    #[test]
    fn sql_mode_is_validated_as_go_does() {
        for (sql, mode) in [
            ("SET sql_mode = 'NOSUCHMODE'", "NOSUCHMODE"),
            (
                "SET sql_mode = 'strict_trans_tables,nosuchmode'",
                "NOSUCHMODE",
            ),
            // Validation comes before the prototype's PAD_CHAR_TO_FULL_LENGTH fence.
            ("SET sql_mode = 'PAD_CHAR_TO_FULL_LENGTH,x'", "X"),
        ] {
            assert_eq!(
                err(sql),
                (
                    1105,
                    "HY000".to_owned(),
                    format!(
                        "ERROR 1231 (42000): Variable 'sql_mode' can't be set to the value of \
                         '{mode}'"
                    )
                ),
                "{sql}"
            );
        }
        for sql in [
            "SET sql_mode = ''",
            "SET sql_mode = 'strict_trans_tables'",
            "SET sql_mode = 'ANSI'",
            "SET @@sql_mode = 'ONLY_FULL_GROUP_BY,NO_ZERO_DATE'",
            "SET sql_mode = DEFAULT",
        ] {
            assert_eq!(set(sql), Ok((None, None)), "{sql}");
        }
        assert_eq!(
            err("SET sql_mode = 'pad_char_to_full_length'"),
            (
                1235,
                "42000".to_owned(),
                "This version of TiDB doesn't yet support 'this sql_mode \
                 (PAD_CHAR_TO_FULL_LENGTH changes CHAR results)'"
                    .to_owned()
            )
        );
    }

    /// The other variables the prototype accepts are validated too: Go's errors for an
    /// unknown charset or collation, a value that is not a number.
    #[test]
    fn accepted_variables_are_validated_as_go_does() {
        // `charset.GetCharsetInfo`: an `errors.Errorf`, so `ErrUnknown`.
        assert_eq!(
            err("SET character_set_client = 'nosuch'"),
            (
                1105,
                "HY000".to_owned(),
                "Unknown charset nosuch".to_owned()
            )
        );
        assert_eq!(
            err("SET collation_connection = 'nosuch'"),
            (
                1273,
                "HY000".to_owned(),
                "Unknown collation: 'nosuch'".to_owned()
            )
        );
        assert_eq!(
            err("SET max_execution_time = 'abc'"),
            (
                1232,
                "42000".to_owned(),
                "Incorrect argument type to variable 'max_execution_time'".to_owned()
            )
        );
        assert_eq!(
            set("SET character_set_results = NULL"),
            Ok((None, Some(None)))
        );
        assert_eq!(
            set("SET NAMES latin1, autocommit = 1, character_set_results = 'utf8mb4'"),
            Ok((
                Some(("latin1".to_owned(), "latin1_bin".to_owned())),
                Some(Some("utf8mb4".to_owned()))
            ))
        );
        // Go's `SetExecutor.Next`: the first failing assignment ends the statement with its
        // error; the assignments before it stay applied, the ones after it are not run.
        let (changes, result) = run("SET NAMES latin1, autocommit = 'abc', NAMES ascii");
        let e = result.unwrap_err();
        assert_eq!(
            (e.code, e.state.to_string(), e.message),
            (
                1231,
                "42000".to_owned(),
                "Variable 'autocommit' can't be set to the value of 'abc'".to_owned()
            )
        );
        assert_eq!(
            names_and_results(&changes),
            [(
                Some(("latin1".to_owned(), "latin1_bin".to_owned())),
                Some(Some("latin1".to_owned()))
            )]
        );
        let (changes, result) = run("SET autocommit = 'abc', NAMES latin1");
        assert_eq!(result.unwrap_err().code, 1231);
        assert!(changes.is_empty());
        // `time_zone` is applied (the prototype keeps nothing of it, the router syncs it),
        // then `max_execution_time` fails.
        let (changes, result) = run("SET time_zone = '+01:00', max_execution_time = 'abc'");
        assert_eq!(result.unwrap_err().code, 1232);
        assert_eq!(names_and_results(&changes), [(None, None)]);
        assert_eq!(
            changes[0].values,
            [("time_zone".to_owned(), "+01:00".to_owned())]
        );
    }

    /// What Connector/J, Connector/Python, Go-MySQL-Driver and the `mysql` client SET while
    /// connecting (#505) is accepted and written into the prototype's session variables, as
    /// Go's `SetSystemVar` leaves them.
    #[test]
    fn driver_connect_sets_are_accepted() {
        for sql in [
            "SET NAMES utf8mb4",
            "SET NAMES utf8mb4 COLLATE utf8mb4_general_ci",
            "SET NAMES utf8 COLLATE utf8_general_ci",
            "SET character_set_results = NULL",
            "SET SQL_SELECT_LIMIT=DEFAULT",
            "SET autocommit=1, sql_mode='STRICT_TRANS_TABLES'",
            "SET sql_mode = 'NO_BACKSLASH_ESCAPES,ANSI_QUOTES'",
            "SET time_zone = '+00:00'",
            "SET transaction_isolation = 'REPEATABLE-READ'",
            "SET max_execution_time = 0",
        ] {
            assert!(set(sql).is_ok(), "{sql}: {:?}", set(sql));
        }
        assert_eq!(
            set_values(
                "SET group_concat_max_len = 1048576, foreign_key_checks = 0, \
                 innodb_lock_wait_timeout = 100, sql_select_limit = DEFAULT"
            ),
            (
                vec![
                    ("group_concat_max_len".to_owned(), "1048576".to_owned()),
                    ("foreign_key_checks".to_owned(), "OFF".to_owned()),
                    ("innodb_lock_wait_timeout".to_owned(), "100".to_owned()),
                    (
                        "sql_select_limit".to_owned(),
                        "18446744073709551615".to_owned()
                    ),
                ],
                vec![]
            )
        );
        // The noop variables: Go's `setSysVariable` warns (`ErrSettingNoopVariable`) only
        // when `tidb_enable_noop_variables` is OFF; it is ON by default.
        assert_eq!(
            set_values("SET sql_safe_updates = 0, unique_checks = 0"),
            (
                vec![
                    ("sql_safe_updates".to_owned(), "OFF".to_owned()),
                    ("unique_checks".to_owned(), "OFF".to_owned()),
                ],
                vec![]
            )
        );
        // A removed variable is ignored, as Go's `setSysVariable` does, also with GLOBAL: Go
        // checks the name before the scope.
        for sql in [
            "SET tidb_enable_streaming = 1",
            "SET GLOBAL tidb_enable_streaming = 1",
        ] {
            assert_eq!(set_values(sql), (vec![], vec![]), "{sql}");
        }
    }

    /// Go's validation errors of the connect-time SETs (#505), whole: code, SQLSTATE and
    /// message.
    #[test]
    fn connect_set_errors_are_go_errors() {
        let wrong_type = (
            1232,
            "42000".to_owned(),
            "Incorrect argument type to variable 'max_execution_time'".to_owned(),
        );
        assert_eq!(err("SET max_execution_time = 1.5"), wrong_type);
        assert_eq!(
            err("SET max_execution_time = 99999999999999999999"),
            wrong_type
        );
        assert_eq!(err("SET max_execution_time = -1.5"), wrong_type);
        for sql in ["SET nosuch_var = 1", "SET GLOBAL nosuch_var = 1"] {
            assert_eq!(
                err(sql),
                (
                    1193,
                    "HY000".to_owned(),
                    "Unknown system variable 'nosuch_var'".to_owned()
                ),
                "{sql}"
            );
        }
        assert_eq!(
            err("SET GLOBAL max_execution_time = 1"),
            (
                1235,
                "42000".to_owned(),
                "This version of TiDB doesn't yet support 'SET GLOBAL'".to_owned()
            )
        );
        // Go's `setCharset`: `GetCollationByName` (the grammar takes any name after
        // `SET NAMES cs COLLATE`), then the charset check.
        assert_eq!(
            err("SET NAMES utf8mb4 COLLATE nosuch"),
            (
                1273,
                "HY000".to_owned(),
                "Unknown collation: 'nosuch'".to_owned()
            )
        );
        assert_eq!(
            err("SET NAMES utf8mb4 COLLATE latin1_bin"),
            (
                1253,
                "42000".to_owned(),
                "COLLATION 'latin1_bin' is not valid for CHARACTER SET 'utf8mb4'".to_owned()
            )
        );
        // The parser's charset and collation checks (`CharsetName`, `CollationName`).
        let parse = |sql: &str| {
            let e = tidb_sql::parse_and_lower(sql).unwrap_err();
            (e.code, e.state.to_string(), e.message)
        };
        for sql in ["SET NAMES nosuch", "SET CHARACTER SET nosuch"] {
            assert_eq!(
                parse(sql),
                (
                    1115,
                    "42000".to_owned(),
                    "Unknown character set: 'nosuch'".to_owned()
                ),
                "{sql}"
            );
        }
        assert_eq!(
            parse("SELECT 'a' COLLATE nosuch"),
            (
                1273,
                "HY000".to_owned(),
                "Unknown collation: 'nosuch'".to_owned()
            )
        );
    }

    /// What the prototype does not support yet stays 1235: a `sql_select_limit` it would not
    /// apply; a SET value that is not a literal (needs the expression rewriter's
    /// `rewriteSystemVariable`); a user variable (needs `getvar`/`setvar`).
    #[test]
    fn unported_connect_sets_are_not_supported() {
        let not_supported = |what: &str| {
            (
                1235,
                "42000".to_owned(),
                format!("This version of TiDB doesn't yet support '{what}'"),
            )
        };
        assert_eq!(
            err("SET sql_select_limit = 10"),
            not_supported(
                "sql_select_limit other than 18446744073709551615 (the prototype applies no \
                 select limit)"
            )
        );
        assert_eq!(
            err("SET sql_mode = CONCAT(@@sql_mode, ',STRICT_TRANS_TABLES')"),
            not_supported("this SET value")
        );
        assert_eq!(err("SET @x = 1"), not_supported("user variables"));
    }

    /// The statements of the per-driver connect workloads of `tools/ab` (#505) run on the
    /// prototype in one session, as `ab file` sends them: every SET and SELECT is answered,
    /// except the ones each workload names as an error or as not supported.
    #[test]
    fn driver_connect_workloads_run_on_the_prototype() {
        const DIR: &str = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../tools/ab/workloads/connect"
        );
        let expected_errors: &[(&str, u16)] = &[
            ("SET @@session.autocommit = OFF", 1235),
            ("SET max_execution_time = 1.5", 1232),
            ("SET max_execution_time = 99999999999999999999", 1232),
            ("SET max_execution_time = 'abc'", 1232),
            ("SET NAMES nosuch", 1115),
            ("SET CHARACTER SET nosuch", 1115),
            ("SET NAMES utf8mb4 COLLATE nosuch", 1273),
            ("SET NAMES utf8mb4 COLLATE latin1_bin", 1253),
            ("SET no_such_variable = 1", 1193),
            ("SET autocommit = 2", 1231),
            ("SET group_concat_max_len = 'abc'", 1232),
            (
                "SET sql_mode = CONCAT(@@sql_mode, ',STRICT_TRANS_TABLES')",
                1235,
            ),
            ("SET @x = 1", 1235),
        ];
        let mut files: Vec<_> = std::fs::read_dir(DIR)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        files.sort();
        assert_eq!(files.len(), 6, "{files:?}");
        let mut seen = 0;
        for file in files {
            let text = std::fs::read_to_string(&file).unwrap();
            let body: String = text
                .lines()
                .filter(|l| !l.starts_with('#') && !l.starts_with("--"))
                .collect::<Vec<_>>()
                .join("\n");
            let mut session = validation_session(&[]);
            let mut warnings = Vec::new();
            for sql in body.split(";\n").map(str::trim).filter(|s| !s.is_empty()) {
                let sql = sql.trim_end_matches(';');
                let result = tidb_sql::parse_and_lower(sql).and_then(|l| match l.stmt {
                    Statement::Set(a) => run_set(session.vars_mut(), &a, |_| {}).1,
                    Statement::Select(sel) => constant::ConstEnv {
                        server_version: "8.0.11-TiDB-tidb-rs",
                        current_db: None,
                        user: "root@127.0.0.1",
                        current_user: "root@%",
                        connection_id: 1,
                        have_ssl: "NO",
                        vars: session.vars_mut(),
                        warnings: &mut warnings,
                    }
                    .select(&sel, &[])
                    .map(|_| ()),
                    other => panic!("{sql}: {other:?}"),
                });
                let want = expected_errors.iter().find(|(s, _)| *s == sql);
                match (want, result) {
                    (None, Ok(())) => {}
                    (Some((_, code)), Err(e)) if e.code == *code => seen += 1,
                    (want, got) => panic!("{}: {sql}: want {want:?}, got {got:?}", file.display()),
                }
            }
        }
        assert_eq!(seen, expected_errors.len());
    }

    /// The prototype's session values survive the rebuild of its session when the cluster's
    /// global values load (`Session::default_session`): a value it set stays, a `SET x =
    /// DEFAULT` made over the compiled defaults becomes the cluster's global value, as Go's
    /// `getVarValue` reads the global value of the session's cluster.
    #[test]
    fn session_values_survive_the_rebuild_over_the_cluster_globals() {
        let mut before = validation_session(&[]);
        let Statement::Set(a) = tidb_sql::parse_and_lower(
            "SET NAMES latin1, group_concat_max_len = 2048, max_execution_time = DEFAULT, \
             sql_mode = 'ANSI_QUOTES'",
        )
        .unwrap()
        .stmt
        else {
            unreachable!()
        };
        let mut values: Vec<SessionValue> = Vec::new();
        let (_, result) = run_set(before.vars_mut(), &a, |c| {
            record_session_values(&mut values, c.values, c.from_default)
        });
        result.unwrap();
        assert_eq!(
            before.vars_mut().get_system_var("max_execution_time").0,
            "0"
        );
        let mut after = build_default_session(
            cluster_globals(&[
                ("max_execution_time", "500"),
                ("group_concat_max_len", "4096"),
                ("sql_mode", ""),
            ]),
            &values,
        )
        .unwrap();
        let vars = after.vars_mut();
        for (name, want) in [
            ("character_set_client", "latin1"),
            ("collation_connection", "latin1_bin"),
            ("group_concat_max_len", "2048"),
            ("max_execution_time", "500"),
            ("sql_mode", "ANSI_QUOTES"),
        ] {
            assert_eq!(vars.get_system_var(name).0, want, "{name}");
        }
    }

    /// #541: allowed session variables (`div_precision_increment`, `group_concat_max_len`,
    /// `max_execution_time`, `tidb_enable_noop_functions`, `tidb_skip_isolation_level_check`,
    /// `tidb_enable_prepared_plan_cache`, `tidb_enable_window_function`, `transaction_isolation`,
    /// `tx_isolation`, ...) work in session `SET` and `SET ... = DEFAULT`; scope/unknown/type
    /// errors match Go before any 1235 fence; result-affecting unported variables stay rejected
    /// with 1235 so they are never silently ignored on the PoC path.
    #[test]
    fn issue_541_variables_and_scope_errors() {
        let mut session = validation_session(&[
            ("div_precision_increment", "7"),
            ("group_concat_max_len", "8192"),
            ("max_execution_time", "5000"),
            ("sql_mode", "STRICT_TRANS_TABLES"),
            ("time_zone", "+02:00"),
            ("tidb_enable_noop_functions", "OFF"),
            ("tidb_skip_isolation_level_check", "OFF"),
            ("tidb_enable_prepared_plan_cache", "ON"),
            ("tidb_enable_window_function", "ON"),
        ]);
        // Session SET + dependent validation switches + alias sync.
        let Statement::Set(a) = tidb_sql::parse_and_lower(
            "SET div_precision_increment = 9, group_concat_max_len = 4096, \
             max_execution_time = 2500, tidb_enable_noop_functions = ON, \
             transaction_read_only = 1, tidb_skip_isolation_level_check = ON, \
             tx_isolation = 'SERIALIZABLE', tidb_enable_prepared_plan_cache = OFF, \
             tidb_enable_window_function = OFF, sql_mode = 'ANSI', time_zone = '+08:00'",
        )
        .unwrap()
        .stmt
        else {
            unreachable!()
        };
        run_set(session.vars_mut(), &a, |_| {}).1.unwrap();
        assert!(!session.vars().enable_window_function);
        for (name, want) in [
            ("div_precision_increment", "9"),
            ("group_concat_max_len", "4096"),
            ("max_execution_time", "2500"),
            ("tidb_enable_noop_functions", "ON"),
            ("transaction_read_only", "ON"),
            ("tx_read_only", "ON"),
            ("tidb_skip_isolation_level_check", "ON"),
            ("transaction_isolation", "SERIALIZABLE"),
            ("tx_isolation", "SERIALIZABLE"),
            ("tidb_enable_prepared_plan_cache", "OFF"),
            ("tidb_enable_window_function", "OFF"),
            ("time_zone", "+08:00"),
        ] {
            assert_eq!(session.vars_mut().get_system_var(name).0, want, "{name}");
        }
        // SET DEFAULT returns to cluster global values.
        let Statement::Set(def) = tidb_sql::parse_and_lower(
            "SET div_precision_increment = DEFAULT, group_concat_max_len = DEFAULT, \
             max_execution_time = DEFAULT, tidb_enable_prepared_plan_cache = DEFAULT, \
             tidb_enable_window_function = DEFAULT, sql_mode = DEFAULT, time_zone = DEFAULT",
        )
        .unwrap()
        .stmt
        else {
            unreachable!()
        };
        run_set(session.vars_mut(), &def, |_| {}).1.unwrap();
        assert!(session.vars().enable_window_function);
        for (name, want) in [
            ("div_precision_increment", "7"),
            ("group_concat_max_len", "8192"),
            ("max_execution_time", "5000"),
            ("tidb_enable_prepared_plan_cache", "ON"),
            ("tidb_enable_window_function", "ON"),
            ("sql_mode", "STRICT_TRANS_TABLES"),
            ("time_zone", "+02:00"),
        ] {
            assert_eq!(session.vars_mut().get_system_var(name).0, want, "{name}");
        }

        // Alias default removal: `SET tx_isolation = DEFAULT; SET transaction_isolation = ...`
        // keeps only the later override in `session_values`.
        let mut values: Vec<SessionValue> = Vec::new();
        let mut alias_sess = validation_session(&[]);
        let Statement::Set(alias_set) = tidb_sql::parse_and_lower(
            "SET tx_isolation = DEFAULT, transaction_isolation = 'READ-COMMITTED'",
        )
        .unwrap()
        .stmt
        else {
            unreachable!()
        };
        run_set(alias_sess.vars_mut(), &alias_set, |c| {
            record_session_values(&mut values, c.values, c.from_default);
        })
        .1
        .unwrap();
        assert_eq!(
            values,
            vec![(
                "transaction_isolation".to_owned(),
                Some("READ-COMMITTED".to_owned())
            )]
        );

        // Scope, value-evaluation-order, and unknown variable errors match Go (code, SQLSTATE, message).
        for (sql, want) in [
            (
                "SET port = 4001",
                (1238, "HY000", "Variable 'port' is a read only variable"),
            ),
            (
                "SET GLOBAL port = 4001",
                (1238, "HY000", "Variable 'port' is a read only variable"),
            ),
            (
                "SET port = 'bad'",
                (1238, "HY000", "Variable 'port' is a read only variable"),
            ),
            (
                "SET port = 1 + 1",
                (
                    1235,
                    "42000",
                    "This version of TiDB doesn't yet support 'this SET value'",
                ),
            ),
            (
                "SET GLOBAL version_comment = 'x'",
                (
                    1238,
                    "HY000",
                    "Variable 'version_comment' is a read only variable",
                ),
            ),
            (
                "SET tidb_gc_life_time = '20m'",
                (
                    1229,
                    "HY000",
                    "Variable 'tidb_gc_life_time' is a GLOBAL variable and should be set with SET GLOBAL",
                ),
            ),
            (
                "SET tidb_gc_life_time = DEFAULT",
                (
                    1229,
                    "HY000",
                    "Variable 'tidb_gc_life_time' is a GLOBAL variable and should be set with SET GLOBAL",
                ),
            ),
            (
                "SET GLOBAL timestamp = 1",
                (
                    1228,
                    "HY000",
                    "Variable 'timestamp' is a SESSION variable and can't be used with SET GLOBAL",
                ),
            ),
            (
                "SET GLOBAL max_execution_time = 'bad'",
                (
                    1232,
                    "42000",
                    "Incorrect argument type to variable 'max_execution_time'",
                ),
            ),
            (
                "SET tidb_general_log = 'bad'",
                (
                    1231,
                    "42000",
                    "Variable 'tidb_general_log' can't be set to the value of 'bad'",
                ),
            ),
            (
                "SET GLOBAL max_execution_time = 1",
                (
                    1235,
                    "42000",
                    "This version of TiDB doesn't yet support 'SET GLOBAL'",
                ),
            ),
            (
                "SET tidb_general_log = 1",
                (
                    1235,
                    "42000",
                    "This version of TiDB doesn't yet support 'SET GLOBAL'",
                ),
            ),
            (
                "SET nosuch = 1",
                (1193, "HY000", "Unknown system variable 'nosuch'"),
            ),
        ] {
            assert_eq!(
                err(sql),
                (want.0, want.1.to_owned(), want.2.to_owned()),
                "{sql}"
            );
        }

        // Unlisted / result-affecting variables are NEVER silently accepted on the PoC path.
        for (sql, want) in [
            (
                "SET tidb_isolation_read_engines = 'tikv'",
                (
                    1235,
                    "42000",
                    "This version of TiDB doesn't yet support 'setting system variable 'tidb_isolation_read_engines''",
                ),
            ),
            (
                "SET tidb_enable_ordered_result_mode = ON",
                (
                    1235,
                    "42000",
                    "This version of TiDB doesn't yet support 'setting system variable 'tidb_enable_ordered_result_mode''",
                ),
            ),
            (
                "SET tidb_enable_external_ts_read = ON",
                (
                    1235,
                    "42000",
                    "This version of TiDB doesn't yet support 'setting system variable 'tidb_enable_external_ts_read''",
                ),
            ),
            (
                "SET tidb_low_resolution_tso = ON",
                (
                    1235,
                    "42000",
                    "This version of TiDB doesn't yet support 'setting system variable 'tidb_low_resolution_tso''",
                ),
            ),
            (
                "SET tidb_mem_quota_query = 1048576",
                (
                    1235,
                    "42000",
                    "This version of TiDB doesn't yet support 'setting system variable 'tidb_mem_quota_query''",
                ),
            ),
            (
                "SET tidb_mem_oom_action = 'CANCEL'",
                (
                    1229,
                    "HY000",
                    "Variable 'tidb_mem_oom_action' is a GLOBAL variable and should be set with SET GLOBAL",
                ),
            ),
            (
                "SET GLOBAL tidb_mem_oom_action = 'CANCEL'",
                (
                    1235,
                    "42000",
                    "This version of TiDB doesn't yet support 'SET GLOBAL'",
                ),
            ),
            (
                "SET tidb_enforce_mpp = OFF",
                (
                    1235,
                    "42000",
                    "This version of TiDB doesn't yet support 'setting system variable 'tidb_enforce_mpp''",
                ),
            ),
            (
                "SET tidb_kv_read_timeout = 10",
                (
                    1193,
                    "HY000",
                    "Unknown system variable 'tidb_kv_read_timeout'",
                ),
            ),
            (
                "SET tx_isolation_one_shot = 'READ-COMMITTED'",
                (
                    1235,
                    "42000",
                    "This version of TiDB doesn't yet support 'setting system variable 'tx_isolation_one_shot''",
                ),
            ),
        ] {
            assert_eq!(
                err(sql),
                (want.0, want.1.to_owned(), want.2.to_owned()),
                "{sql}"
            );
        }

        // Fenced session variables stay rejected with 1235.
        for var in router::sync::FENCED_SYSVARS {
            let sql = match *var {
                "tidb_snapshot" => "SET tidb_snapshot = '2020-01-01 00:00:00'".to_owned(),
                "tidb_read_staleness" => "SET tidb_read_staleness = '-5'".to_owned(),
                "tx_read_ts" => "SET tx_read_ts = '1'".to_owned(),
                "tidb_txn_mode" => "SET tidb_txn_mode = 'optimistic'".to_owned(),
                "tidb_replica_read" => "SET tidb_replica_read = 'follower'".to_owned(),
                "tidb_read_consistency" => "SET tidb_read_consistency = 'weak'".to_owned(),
                "tidb_rc_read_check_ts" => "SET tidb_rc_read_check_ts = ON".to_owned(),
                other => format!("SET {other} = DEFAULT"),
            };
            assert_eq!(err(&sql).0, 1235, "{sql}");
        }
    }

    /// `tidb_gc_life_time` reads through `ClusterGlobals::get_tidb_table_value` (`mysql.tidb`).
    #[test]
    fn tidb_table_vars_and_charset_results_null() {
        let tidb_map: HashMap<String, String> =
            [("tikv_gc_life_time".to_owned(), "24h0m0s".to_owned())]
                .into_iter()
                .collect();
        let tidb_table = Arc::new(parking_lot::RwLock::new(tidb_map));
        let globals = cluster_globals(&[]).with_tidb_table(tidb_table);
        let mut s = build_default_session(globals, &[]).unwrap();
        let Statement::Set(a) =
            tidb_sql::parse_and_lower("SET NAMES latin1, character_set_results = NULL")
                .unwrap()
                .stmt
        else {
            unreachable!()
        };
        let mut results_cs = Some("utf8mb4".to_owned());
        run_set(s.vars_mut(), &a, |c| {
            if let Some(r) = c.results {
                results_cs = r;
            }
        })
        .1
        .unwrap();
        assert_eq!(results_cs, None);

        let Statement::Select(sel) =
            tidb_sql::parse_and_lower("SELECT @@version_comment, @@global.tidb_gc_life_time")
                .unwrap()
                .stmt
        else {
            unreachable!()
        };
        let mut warnings = Vec::new();
        let mut rs = constant::ConstEnv {
            server_version: "8.0.11-TiDB-tidb-rs",
            current_db: None,
            user: "root@127.0.0.1",
            current_user: "root@%",
            connection_id: 1,
            have_ssl: "DISABLED",
            vars: s.vars_mut(),
            warnings: &mut warnings,
        }
        .select(&sel, &[])
        .unwrap();
        for c in &mut rs.columns {
            apply_result_charset(c, results_cs.as_deref());
        }
        assert_eq!(rs.columns[0].charset, 47);
        assert_eq!(rs.rows[0][1], Value::from("24h0m0s"));
    }

    /// `--new-stack=off` parses queries and prepares with the session's `sql_mode` (both after
    /// `SET sql_mode` and after `DEFAULT` over cluster globals).
    #[test]
    fn off_mode_parse_options_follow_session_sql_mode() {
        let mut s = validation_session(&[("sql_mode", "ANSI_QUOTES")]);
        let opts = |sess: &tidb_session::Session| tidb_parser::ParseOptions {
            window_functions: sess.vars().enable_window_function,
            sql_mode: tidb_base::mysql::SQLMode(sess.vars().sql_mode),
        };

        // Starts with cluster global ANSI_QUOTES.
        let lowered = tidb_sql::parse_and_lower_with(r#"SELECT "x""#, &opts(&s)).unwrap();
        let Statement::Select(sel) = lowered.stmt else {
            panic!("expected Select");
        };
        assert!(matches!(
            sel.fields[0],
            tidb_sql::ir::Field::Expr {
                expr: tidb_sql::ir::Expr::Column(_),
                ..
            }
        ));

        // SET sql_mode = 'NO_BACKSLASH_ESCAPES,IGNORE_SPACE,REAL_AS_FLOAT,PIPES_AS_CONCAT,HIGH_NOT_PRECEDENCE'.
        run_over(
            &mut s,
            "SET sql_mode = 'NO_BACKSLASH_ESCAPES,IGNORE_SPACE,REAL_AS_FLOAT,PIPES_AS_CONCAT,HIGH_NOT_PRECEDENCE'",
        )
        .1
        .unwrap();
        let o = opts(&s);
        let lowered = tidb_sql::parse_and_lower_with(r"SELECT '\n'", &o).unwrap();
        let Statement::Select(sel) = lowered.stmt else {
            panic!("expected Select");
        };
        assert!(matches!(
            &sel.fields[0],
            tidb_sql::ir::Field::Expr {
                expr: tidb_sql::ir::Expr::Literal(Literal::Str(str_val)),
                ..
            } if str_val == r"\n"
        ));
        assert!(tidb_sql::ir::parse_one_with("SELECT CAST (1 AS REAL)", &o).is_ok());
        assert_eq!(
            tidb_sql::parse_and_lower_with("SELECT 'a' || 'b'", &o)
                .unwrap_err()
                .code,
            1235
        );
        let stmt = tidb_sql::ir::parse_one_with("SELECT NOT 1 BETWEEN -5 AND 5", &o).unwrap();
        assert!(matches!(
            stmt,
            tidb_parser::ast::Stmt::Select(sel) if matches!(
                sel.fields.fields[0].expr.as_ref(),
                Some(tidb_parser::ast::Expr::Between(b))
                    if matches!(&b.expr, tidb_parser::ast::Expr::Unary(u) if u.op == tidb_parser::ast::Op::Not2)
            )
        ));

        // SET sql_mode = DEFAULT restores the cluster's global ANSI_QUOTES.
        run_over(&mut s, "SET sql_mode = DEFAULT").1.unwrap();
        let stmt = tidb_sql::ir::parse_one_with(r#"SELECT "x""#, &opts(&s)).unwrap();
        assert!(matches!(
            stmt,
            tidb_parser::ast::Stmt::Select(sel)
                if matches!(sel.fields.fields[0].expr.as_ref(), Some(tidb_parser::ast::Expr::Column(_)))
        ));
    }

    /// #509 / #555 (`--new-stack=off`): `PREPARE -> EXECUTE -> SET sql_mode -> EXECUTE ->
    /// CLOSE -> re-PREPARE -> EXECUTE` keeps the prepare-time lowered statement for the first
    /// handle and parses afresh under the updated `sql_mode` on re-prepare.
    #[test]
    fn off_mode_prepare_set_sql_mode_execute_reprepare() {
        let mut s = validation_session(&[]);
        let opts = |sess: &tidb_session::Session| tidb_parser::ParseOptions {
            window_functions: sess.vars().enable_window_function,
            sql_mode: tidb_base::mysql::SQLMode(sess.vars().sql_mode),
        };
        let eval_const_select =
            |sess: &mut tidb_session::Session, lowered: &Lowered| -> Result<Vec<u8>, SqlError> {
                let Statement::Select(sel) = &lowered.stmt else {
                    panic!("expected Select");
                };
                let mut warnings = Vec::new();
                let rs = constant::ConstEnv {
                    server_version: "8.0.11-TiDB-tidb-rs",
                    current_db: None,
                    user: "root@127.0.0.1",
                    current_user: "root@%",
                    connection_id: 1,
                    have_ssl: "NO",
                    vars: sess.vars_mut(),
                    warnings: &mut warnings,
                }
                .select(sel, &[])?;
                match &rs.rows[0][0] {
                    Value::Bytes(b) => Ok(b.clone()),
                    other => panic!("unexpected value: {other:?}"),
                }
            };

        // 1. NO_BACKSLASH_ESCAPES: PREPARE -> EXECUTE -> SET '' -> EXECUTE -> re-PREPARE -> EXECUTE.
        run_over(&mut s, "SET sql_mode = 'NO_BACKSLASH_ESCAPES'")
            .1
            .unwrap();
        let stmt1 = tidb_sql::parse_and_lower_with(r"SELECT '\n'", &opts(&s)).unwrap();
        assert_eq!(eval_const_select(&mut s, &stmt1).unwrap(), b"\\n");

        run_over(&mut s, "SET sql_mode = ''").1.unwrap();
        assert_eq!(eval_const_select(&mut s, &stmt1).unwrap(), b"\\n");

        let stmt2 = tidb_sql::parse_and_lower_with(r"SELECT '\n'", &opts(&s)).unwrap();
        assert_eq!(eval_const_select(&mut s, &stmt2).unwrap(), b"\n");

        // 2. ANSI_QUOTES: PREPARE `SELECT "abc"` under '' -> EXECUTE ('abc') -> SET ANSI_QUOTES
        //    -> EXECUTE ('abc') -> re-PREPARE -> 1054 Unknown column 'abc'.
        let stmt3 = tidb_sql::parse_and_lower_with(r#"SELECT "abc""#, &opts(&s)).unwrap();
        assert_eq!(eval_const_select(&mut s, &stmt3).unwrap(), b"abc");

        run_over(&mut s, "SET sql_mode = 'ANSI_QUOTES'").1.unwrap();
        assert_eq!(eval_const_select(&mut s, &stmt3).unwrap(), b"abc");

        let stmt4 = tidb_sql::parse_and_lower_with(r#"SELECT "abc""#, &opts(&s)).unwrap();
        let err = eval_const_select(&mut s, &stmt4).unwrap_err();
        assert_eq!(err.code, 1054, "{}", err.message);
    }
}
