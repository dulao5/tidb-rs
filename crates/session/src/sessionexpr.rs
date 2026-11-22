//! go-file: pkg/expression/sessionexpr/sessionctx.go
//!
//! The session's expression context (`sessionctx.Context.GetExprCtx()`): the `ExprContext` /
//! `EvalContext` over the live session (D05.3 tasks P1-1, inherited from D00.1). Spec:
//! `specs/D05.3-session/spec.md` §8.
//!
//! Go keeps one `ExprContext` per session that reads `SessionVars` on every call. Under N2 the
//! expression context is `Send + Sync` and shared (`Arc`), while the session owns its
//! variables (`&mut`), so the context cannot hold the session. It holds a [`SessionView`]
//! built from the session when the session hands the context out (`GetExprCtx` /
//! `GetPlanCtx`, once per statement), in the three layers of N2 §3.1:
//!
//! - the scalar session variables the context reads (SQL mode, current database, charset,
//!   `max_allowed_packet`, ...): a snapshot taken when the view is built. Nothing on the new
//!   stack changes them while a statement runs (N2 §3.2; `SET_VAR` hints, which would, are
//!   a boundary of the hint parser, D02.2);
//! - what Go changes while the statement runs and reads back through the session: the
//!   parameter values of an EXECUTE (`SessionVars.PlanCacheParams`, set by the plan cache's
//!   miss path inside `Optimize`), the plan column id counter, the `RAND()` generator and the
//!   user variables. These are cells shared with `SessionVars` (`SharedField`,
//!   `AtomicI64Field::share`, `Arc<UserVars>`), so the context sees the session's current
//!   values;
//! - the statement context: the context keeps the statement's warning sink, plan cache
//!   tracker and stale TSO provider (shared handles) and copies its id, type context and
//!   error context. It does not keep the `StatementContext` itself: the planner writes its
//!   plain fields through `SessionVars::stmt_ctx_mut`, which needs the context unshared.
//!
//! Optional properties (`NewEvalContext`): Go registers all nine. Registered here: the
//! InfoSchema, the store, the SQL executor and the sequence operator (both answer a boundary
//! error: the restricted SQL executor and sequences are not ported), the DDL owner
//! (`false`: tidb-rs never takes part in the DDL owner election) and the privilege checker
//! (this context; there is no privilege manager, so every check passes as in Go's
//! nil-manager branch). Not registered (spec §8.3): the current user (the Rust
//! `SessionVars` has no `User` / `ActiveRoles`, the privilege block D04.1), the session
//! variables provider (D05.3 tasks P1-2) and the advisory locks (the session's lock table is
//! not ported). A builtin that needs one of them gets Go's "optional property not exists"
//! error; none of them is on the first vertical slice.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use parking_lot::Mutex;
use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_base::contextutil::{PlanCacheTracker, SQLWarn, WarnAppender, WarnHandler};
use tidb_base::gostd::time::{Location, Time};
use tidb_base::mysql::{PrivilegeType, SQLMode};
use tidb_kv::kv::Storage;
use tidb_sessionctx::errctx;
use tidb_sessionctx::exprctx::{
    BuildContext, EvalContext as EvalContextTrait, EvalContextRef, ExprContext as ExprContextTrait,
    MysqlRngRef, OptionalEvalPropKey, OptionalEvalPropKeySet, OptionalEvalPropProviderRef,
    ParamValues, StaticConvertibleEvalContext, StaticConvertibleEvalContextRef,
    StaticConvertibleExprContext, WarnHandlerRef, err_param_index_exceed_param_counts,
};
use tidb_sessionctx::expropt::{
    CurrentUserPropProvider, DDLOwnerInfoProvider, InfoSchemaPropProvider, KVStorePropProvider,
    MetaOnlyInfoSchema, MetaOnlyInfoSchemaRef, OptionalEvalPropProviders, PrivilegeChecker,
    PrivilegeCheckerProvider, SQLExecutorPropProvider, SQLExecutorRef, SequenceOperatorProvider,
    SequenceOperatorRef,
};
use tidb_sessionctx::exprstatic;
use tidb_sessionctx::stmtctx::{StaleTSOSource, StmtWarnSink};
use tidb_sessionctx::variable::names::Timestamp;
use tidb_sessionctx::variable::plan_cache_params::PlanCacheParamList;
use tidb_sessionctx::variable::session::SessionVars;
use tidb_sessionctx::variable::tidb_defs::{DefTimestamp, DefaultWeekFormat};
use tidb_sessionctx::variable::uservars::{UserVars, UserVarsReaderRef};
use tidb_types::context::Context as TypeContext;
use tidb_types::datum::Datum;

use tidb_sessiontxn::sctx::InfoSchemaRef;

/// Go `variable.BlockEncryptionMode`.
#[allow(non_upper_case_globals)]
const BlockEncryptionMode: &str = "block_encryption_mode";

/// What the expression context reads of the session (Go's `sctx sessionctx.Context`, read
/// through `GetSessionVars()` and the session's methods): see the module doc for the three
/// layers.
// glue: N2 (the session as sessionexpr reads it: snapshot, shared cells, statement handles)
pub struct SessionView {
    // The scalar session variables, as of the view's build.
    sql_mode: SQLMode,
    current_db: String,
    max_allowed_packet: u64,
    enable_redact_log: String,
    /// `GetSystemVar(DefaultWeekFormat)` (value, found).
    default_week_format: (String, bool),
    div_precision_increment: i64,
    charset_info: (String, String),
    default_collation_for_utf8mb4: String,
    /// `GetSystemVar(BlockEncryptionMode)` (Go ignores whether it is found).
    block_encryption_mode: String,
    sysdate_is_now: bool,
    noop_funcs_mode: i64,
    windowing_use_high_precision: bool,
    group_concat_max_len: u64,
    connection_id: u64,
    /// The session's `timestamp` variable when it is set (`SET timestamp = ...`); `None` is
    /// the default, whose value is the current time.
    timestamp: Option<String>,
    /// `GetPlanCtx().GetReadonlyUserVarMap()`: the user variables the preprocessor found
    /// read-only in this statement.
    readonly_user_vars: HashSet<String>,

    // The statement context (`SessionVars.StmtCtx`).
    ctx_id: u64,
    type_ctx: TypeContext,
    err_ctx: errctx::Context,
    in_show_warning: bool,
    warn_sink: Arc<StmtWarnSink>,
    plan_cache_tracker: Arc<PlanCacheTracker>,
    stale_tso: StaleTSOSource,

    // The cells shared with the session variables.
    user_vars: Arc<UserVars>,
    plan_cache_params: Arc<Mutex<PlanCacheParamList>>,
    plan_column_id: Arc<AtomicI64>,
    rng: MysqlRngRef,

    // The session's services.
    store: Arc<dyn Storage>,
    info_schema: InfoSchemaRef,
    domain_info_schema: InfoSchemaRef,
}

impl SessionView {
    /// The view of the session whose variables are `vars`, read-only variable map
    /// `readonly_user_vars`, store `store` and InfoSchemas (`GetInfoSchema()`,
    /// `GetDomainInfoSchema()`).
    // glue: N2 (builds the view of the session sessionexpr reads)
    pub fn new(
        vars: &SessionVars,
        readonly_user_vars: &HashSet<String>,
        store: Arc<dyn Storage>,
        info_schema: InfoSchemaRef,
        domain_info_schema: InfoSchemaRef,
    ) -> SessionView {
        let sc = vars.stmt_ctx();
        let (timestamp, found) = vars.get_system_var(Timestamp);
        SessionView {
            sql_mode: SQLMode(vars.sql_mode),
            current_db: vars.current_db.clone(),
            max_allowed_packet: vars.max_allowed_packet,
            enable_redact_log: vars.enable_redact_log.clone(),
            default_week_format: vars.get_system_var(DefaultWeekFormat),
            div_precision_increment: vars.get_div_precision_increment(),
            charset_info: vars.get_charset_info(),
            default_collation_for_utf8mb4: vars.default_collation_for_utf8mb4.clone(),
            block_encryption_mode: vars.get_system_var(BlockEncryptionMode).0,
            sysdate_is_now: vars.sysdate_is_now,
            noop_funcs_mode: vars.noop_funcs_mode,
            windowing_use_high_precision: vars.windowing_use_high_precision,
            group_concat_max_len: vars.group_concat_max_len,
            connection_id: vars.connection_id,
            timestamp: (found && timestamp != DefTimestamp).then_some(timestamp),
            readonly_user_vars: readonly_user_vars.clone(),
            ctx_id: sc.ctx_id(),
            type_ctx: sc.type_ctx(),
            err_ctx: sc.err_ctx(),
            in_show_warning: sc.in_show_warning,
            warn_sink: sc.warn_appender(),
            plan_cache_tracker: Arc::clone(&sc.plan_cache_tracker),
            stale_tso: sc.stale_tso_source(),
            user_vars: Arc::clone(&vars.state.user_vars),
            plan_cache_params: vars.state.plan_cache_params.share(),
            plan_column_id: vars.state.plan_column_id.share(),
            rng: vars.state.rng.share(),
            store,
            info_schema,
            domain_info_schema,
        }
    }
}

/// ExprContext implements `ExprContext`
pub struct ExprContext {
    sctx: Arc<SessionView>,
    eval_ctx: Arc<EvalContext>,
}

/// NewExprContext creates a new ExprContext.
///
/// Go builds it once per session; here the session builds it from the [`SessionView`] each
/// time it hands the context out (module doc).
// go: NewExprContext L2 (built per statement over a view of the session, N2)
pub fn new_expr_context(sctx: SessionView) -> Arc<ExprContext> {
    let sctx = Arc::new(sctx);
    Arc::new(ExprContext {
        eval_ctx: new_eval_context(Arc::clone(&sctx)),
        sctx,
    })
}

impl ExprContext {
    /// IntoStatic turns the ExprContext into a ExprContext.
    // go: ExprContext.IntoStatic L1
    pub fn into_static(&self) -> Arc<exprstatic::ExprContext> {
        exprstatic::make_expr_context_static(self)
    }

    /// The `*EvalContext` Go embeds.
    // glue: Go struct embedding of *EvalContext
    pub fn eval(&self) -> &Arc<EvalContext> {
        &self.eval_ctx
    }
}

impl BuildContext for ExprContext {
    /// GetEvalCtx returns the EvalContext.
    // go: ExprContext.GetEvalCtx L1
    fn get_eval_ctx(&self) -> EvalContextRef {
        Arc::clone(&self.eval_ctx) as EvalContextRef
    }

    /// GetCharsetInfo gets charset and collation for current context.
    // go: ExprContext.GetCharsetInfo L1
    fn get_charset_info(&self) -> (String, String) {
        self.sctx.charset_info.clone()
    }

    /// GetDefaultCollationForUTF8MB4 returns the default collation of UTF8MB4.
    // go: ExprContext.GetDefaultCollationForUTF8MB4 L1
    fn get_default_collation_for_utf8mb4(&self) -> String {
        self.sctx.default_collation_for_utf8mb4.clone()
    }

    /// GetBlockEncryptionMode returns the variable block_encryption_mode
    // go: ExprContext.GetBlockEncryptionMode L1
    fn get_block_encryption_mode(&self) -> String {
        self.sctx.block_encryption_mode.clone()
    }

    /// GetSysdateIsNow returns a bool to determine whether Sysdate is an alias of Now function.
    /// It is the value of variable `tidb_sysdate_is_now`.
    // go: ExprContext.GetSysdateIsNow L1
    fn get_sysdate_is_now(&self) -> bool {
        self.sctx.sysdate_is_now
    }

    /// GetNoopFuncsMode returns the noop function mode: OFF/ON/WARN values as 0/1/2.
    // go: ExprContext.GetNoopFuncsMode L1
    fn get_noop_funcs_mode(&self) -> i64 {
        self.sctx.noop_funcs_mode
    }

    /// Rng is used to generate random values.
    // go: ExprContext.Rng L1
    fn rng(&self) -> MysqlRngRef {
        Arc::clone(&self.sctx.rng)
    }

    /// IsUseCache indicates whether to cache the build expression in plan cache.
    /// If SetSkipPlanCache is invoked, it should return false.
    // go: ExprContext.IsUseCache L1
    fn is_use_cache(&self) -> bool {
        self.sctx.plan_cache_tracker.use_cache()
    }

    /// SetSkipPlanCache sets to skip the plan cache and records the reason.
    // go: ExprContext.SetSkipPlanCache L1
    fn set_skip_plan_cache(&self, reason: &str) {
        self.sctx.plan_cache_tracker.set_skip_plan_cache(reason);
    }

    /// AllocPlanColumnID allocates column id for plan.
    // go: ExprContext.AllocPlanColumnID L1
    fn alloc_plan_column_id(&self) -> i64 {
        // SessionVars.AllocPlanColumnID: Go's atomic.Int64.Add wraps on overflow.
        self.sctx
            .plan_column_id
            .fetch_add(1, Ordering::SeqCst)
            .wrapping_add(1)
    }

    /// IsInNullRejectCheck returns whether the expression is in null reject check.
    // go: ExprContext.IsInNullRejectCheck L1
    fn is_in_null_reject_check(&self) -> bool {
        false
    }

    /// IsConstantPropagateCheck returns whether the ctx is in constant propagate check.
    // go: ExprContext.IsConstantPropagateCheck L1
    fn is_constant_propagate_check(&self) -> bool {
        false
    }

    /// ConnectionID indicates the connection ID of the current session.
    /// If the context is not in a session, it should return 0.
    // go: ExprContext.ConnectionID L1
    fn connection_id(&self) -> u64 {
        self.sctx.connection_id
    }

    /// IsReadonlyUserVar checks whether the user variable is readonly.
    // go: ExprContext.IsReadonlyUserVar L1
    fn is_readonly_user_var(&self, name: &str) -> bool {
        self.sctx.readonly_user_vars.contains(name)
    }
}

impl ExprContextTrait for ExprContext {
    /// GetWindowingUseHighPrecision determines whether to compute window operations without
    /// loss of precision. see
    /// https://dev.mysql.com/doc/refman/8.0/en/window-function-optimization.html for more
    /// details.
    // go: ExprContext.GetWindowingUseHighPrecision L1
    fn get_windowing_use_high_precision(&self) -> bool {
        self.sctx.windowing_use_high_precision
    }

    /// GetGroupConcatMaxLen returns the value of the 'group_concat_max_len' system variable.
    // go: ExprContext.GetGroupConcatMaxLen L1
    fn get_group_concat_max_len(&self) -> u64 {
        self.sctx.group_concat_max_len
    }
}

impl StaticConvertibleExprContext for ExprContext {
    /// GetStaticConvertibleEvalContext implements context.StaticConvertibleExprContext.
    // go: ExprContext.GetStaticConvertibleEvalContext L1
    fn get_static_convertible_eval_context(&self) -> StaticConvertibleEvalContextRef {
        Arc::clone(&self.eval_ctx) as StaticConvertibleEvalContextRef
    }

    /// GetPlanCacheTracker implements context.StaticConvertibleExprContext.
    // go: ExprContext.GetPlanCacheTracker L1
    fn get_plan_cache_tracker(&self) -> Arc<PlanCacheTracker> {
        Arc::clone(&self.sctx.plan_cache_tracker)
    }

    /// GetLastPlanColumnID implements context.StaticConvertibleExprContext.
    // go: ExprContext.GetLastPlanColumnID L1
    fn get_last_plan_column_id(&self) -> i64 {
        self.sctx.plan_column_id.load(Ordering::SeqCst)
    }
}

/// EvalContext implements the `expression.EvalContext` interface to provide evaluation
/// context in session.
pub struct EvalContext {
    sctx: Arc<SessionView>,
    props: OptionalEvalPropProviders,
}

/// NewEvalContext creates a new EvalContext.
///
/// The providers Go registers that the Rust session cannot back are left out (module doc,
/// spec §8.3), so the property set is not full: Go's `intest.Assert` on it is compiled out
/// of production builds and is not ported.
// go: NewEvalContext L2 (the current user, session variables and advisory lock providers are not registered: spec §8.3)
pub fn new_eval_context(sctx: Arc<SessionView>) -> Arc<EvalContext> {
    Arc::new_cyclic(|me: &std::sync::Weak<EvalContext>| {
        let mut ctx = EvalContext {
            sctx: Arc::clone(&sctx),
            props: OptionalEvalPropProviders::default(),
        };
        // set all optional properties
        if let Ok(p) = current_user_prop() {
            ctx.set_optional_prop(Arc::new(p));
        }
        // ctx.setOptionalProp(expropt.NewSessionVarsProvider(sctx)): not registered (P1-2).
        ctx.set_optional_prop(Arc::new(info_schema_prop(Arc::clone(&sctx))));
        let store = Arc::clone(&sctx.store);
        ctx.set_optional_prop(Arc::new(KVStorePropProvider(Arc::new(move || {
            Arc::clone(&store)
        }))));
        ctx.set_optional_prop(Arc::new(sql_executor_prop()));
        ctx.set_optional_prop(Arc::new(sequence_operator_prop()));
        // ctx.setOptionalProp(expropt.NewAdvisoryLockPropProvider(sctx)): not registered.
        ctx.set_optional_prop(Arc::new(DDLOwnerInfoProvider(Arc::new(is_ddl_owner))));
        let me = std::sync::Weak::clone(me);
        ctx.set_optional_prop(Arc::new(PrivilegeCheckerProvider(Arc::new(move || {
            me.upgrade().expect("the context outlives its providers") as Arc<dyn PrivilegeChecker>
        }))));
        ctx
    })
}

/// `sctx.IsDDLOwner`: tidb-rs never takes part in the DDL owner election (DDL runs on Go
/// TiDB), so this instance is never the owner.
// glue: session.IsDDLOwner (no DDL owner manager in tidb-rs)
fn is_ddl_owner() -> bool {
    false
}

impl EvalContext {
    // go: EvalContext.setOptionalProp L1
    fn set_optional_prop(&mut self, prop: OptionalEvalPropProviderRef) {
        debug_assert!(!self.props.contains(prop.desc().key()));
        self.props.add(prop);
    }

    /// Sctx returns the innert session context (here the view of the session the context
    /// was built from, module doc).
    // go: EvalContext.Sctx L2 (the view of the session, N2)
    pub fn sctx(&self) -> &SessionView {
        &self.sctx
    }

    /// IntoStatic turns the EvalContext into a EvalContext.
    // go: EvalContext.IntoStatic L1
    pub fn into_static(self: &Arc<Self>) -> Arc<exprstatic::EvalContext> {
        exprstatic::make_eval_context_static(&(Arc::clone(self) as StaticConvertibleEvalContextRef))
    }
}

impl WarnAppender for EvalContext {
    /// AppendWarning append warnings to the context.
    // go: EvalContext.AppendWarning L1
    fn append_warning(&self, err: Error) {
        self.sctx.warn_sink.handler().append_warning(err);
    }

    /// AppendNote appends notes to the context.
    // go: EvalContext.AppendNote L1
    fn append_note(&self, err: Error) {
        self.sctx.warn_sink.handler().append_note(err);
    }
}

impl WarnHandler for EvalContext {
    /// WarningCount gets warning count.
    // go: EvalContext.WarningCount L1
    fn warning_count(&self) -> i64 {
        // StmtCtx.WarningCount(): 0 while SHOW WARNINGS runs.
        if self.sctx.in_show_warning {
            return 0;
        }
        i64::from(self.sctx.warn_sink.handler().warning_count() as u16)
    }

    /// TruncateWarnings truncates warnings begin from start and returns the truncated warnings.
    // go: EvalContext.TruncateWarnings L1
    fn truncate_warnings(&self, start: i64) -> Vec<SQLWarn> {
        self.sctx.warn_sink.handler().truncate_warnings(start)
    }

    /// CopyWarnings copies the warnings to dst
    // go: EvalContext.CopyWarnings L1
    fn copy_warnings(&self, dst: Vec<SQLWarn>) -> Vec<SQLWarn> {
        self.sctx.warn_sink.handler().copy_warnings(dst)
    }
}

impl ParamValues for EvalContext {
    /// GetParamValue returns the value of the parameter by index.
    // go: EvalContext.GetParamValue L1
    fn get_param_value(&self, idx: i64) -> Result<Datum, Error> {
        let params = self.sctx.plan_cache_params.lock();
        let params = params.all_param_values();
        // Go indexes with an int that is never negative here; a negative index is out of
        // range too.
        match usize::try_from(idx).ok().and_then(|i| params.get(i)) {
            Some(d) => Ok(d.clone()),
            None => Err(err_param_index_exceed_param_counts()),
        }
    }
}

impl EvalContextTrait for EvalContext {
    /// CtxID returns the context id.
    // go: EvalContext.CtxID L1
    fn ctx_id(&self) -> u64 {
        self.sctx.ctx_id
    }

    /// SQLMode returns the sql mode
    // go: EvalContext.SQLMode L1
    fn sql_mode(&self) -> SQLMode {
        self.sctx.sql_mode
    }

    /// TypeCtx returns the types.Context
    ///
    /// Go's `intest` assertion `exprctx.AssertLocationWithSessionVars` is not made: the
    /// statement context's location is set by `ResetContextOfStmt` only, so a session that
    /// has not run a statement has the zero context's UTC (the stand-in `StatementContext`,
    /// as in the Rust `assert_location_with_session_vars`).
    // go: EvalContext.TypeCtx L2 (the intest location assertion is not made)
    fn type_ctx(&self) -> TypeContext {
        self.sctx.type_ctx.clone()
    }

    /// ErrCtx returns the errctx.Context
    // go: EvalContext.ErrCtx L1
    fn err_ctx(&self) -> errctx::Context {
        self.sctx.err_ctx.clone()
    }

    /// Location returns the timezone info
    // go: EvalContext.Location L1
    fn location(&self) -> Arc<Location> {
        let tc = self.type_ctx();
        tc.location()
    }

    /// CurrentDB returns the current database name
    // go: EvalContext.CurrentDB L1
    fn current_db(&self) -> String {
        self.sctx.current_db.clone()
    }

    /// CurrentTime returns the current time
    // go: EvalContext.CurrentTime L1
    fn current_time(&self) -> Result<Time, Error> {
        get_stmt_timestamp(Some(&self.sctx))
    }

    /// GetMaxAllowedPacket returns the value of the 'max_allowed_packet' system variable.
    // go: EvalContext.GetMaxAllowedPacket L1
    fn get_max_allowed_packet(&self) -> u64 {
        self.sctx.max_allowed_packet
    }

    /// GetTiDBRedactLog returns the value of the 'tidb_redact_log' system variable.
    // go: EvalContext.GetTiDBRedactLog L1
    fn get_tidb_redact_log(&self) -> String {
        self.sctx.enable_redact_log.clone()
    }

    /// GetDefaultWeekFormatMode returns the value of the 'default_week_format' system variable.
    // go: EvalContext.GetDefaultWeekFormatMode L1
    fn get_default_week_format_mode(&self) -> String {
        let (mode, ok) = &self.sctx.default_week_format;
        if !ok || mode.is_empty() {
            return "0".to_owned();
        }
        mode.clone()
    }

    /// GetDivPrecisionIncrement returns the specified value of DivPrecisionIncrement.
    // go: EvalContext.GetDivPrecisionIncrement L1
    fn get_div_precision_increment(&self) -> i64 {
        self.sctx.div_precision_increment
    }

    /// GetUserVarsReader returns the user variables.
    // go: EvalContext.GetUserVarsReader L1
    fn get_user_vars_reader(&self) -> UserVarsReaderRef {
        Arc::clone(&self.sctx.user_vars) as UserVarsReaderRef
    }

    /// GetOptionalPropSet gets the optional property set from context
    // go: EvalContext.GetOptionalPropSet L1
    fn get_optional_prop_set(&self) -> OptionalEvalPropKeySet {
        self.props.prop_key_set()
    }

    /// GetOptionalPropProvider gets the optional property provider by key
    // go: EvalContext.GetOptionalPropProvider L1
    fn get_optional_prop_provider(
        &self,
        key: OptionalEvalPropKey,
    ) -> Option<OptionalEvalPropProviderRef> {
        self.props.get(key)
    }
}

impl PrivilegeChecker for EvalContext {
    /// RequestVerification verifies user privilege
    ///
    /// `privilege.GetPrivilegeManager(ctx.sctx)` is nil: tidb-rs has no privilege manager
    /// (D04.1, VS1 plan §3.2), so Go's nil branch is taken.
    // go: EvalContext.RequestVerification L1
    fn request_verification(
        &self,
        _db: &str,
        _table: &str,
        _column: &str,
        _priv: PrivilegeType,
    ) -> bool {
        // checker == nil
        true
    }

    /// RequestDynamicVerification verifies user privilege for a DYNAMIC privilege.
    ///
    /// No privilege manager, as [`PrivilegeChecker::request_verification`].
    // go: EvalContext.RequestDynamicVerification L1
    fn request_dynamic_verification(&self, _priv_name: &str, _grantable: bool) -> bool {
        // checker == nil
        true
    }
}

impl StaticConvertibleEvalContext for EvalContext {
    /// AllParamValues implements context.StaticConvertibleEvalContext.
    // go: EvalContext.AllParamValues L1
    fn all_param_values(&self) -> Vec<Datum> {
        self.sctx
            .plan_cache_params
            .lock()
            .all_param_values()
            .to_vec()
    }

    /// GetWarnHandler implements context.StaticConvertibleEvalContext.
    // go: EvalContext.GetWarnHandler L1
    fn get_warn_handler(&self) -> WarnHandlerRef {
        self.sctx.warn_sink.handler() as WarnHandlerRef
    }
}

/// `getStmtTimestamp`: the stale read TSO's time when the statement reads stale data, else
/// the session's `timestamp` variable (the current time when it is not set).
///
/// Go logs the error of `GetStaleTSO` and goes on; the logger here is `log`.
// go: getStmtTimestamp L1
fn get_stmt_timestamp(ctx: Option<&SessionView>) -> Result<Time, Error> {
    if let Some(ctx) = ctx {
        match ctx.stale_tso.get_stale_tso() {
            Ok(stale_tso) if stale_tso != 0 => {
                let t = tidb_kv::txn::get_time_from_ts(stale_tso)
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default();
                return Ok(tidb_base::gostd::time::unix(
                    t.as_secs() as i64,
                    i64::from(t.subsec_nanos()),
                ));
            }
            Ok(_) => {}
            Err(err) => log::error!("get stale tso failed: {err}"),
        }
    }

    let now = tidb_base::gostd::time::now();

    let Some(ctx) = ctx else {
        return Ok(now);
    };

    // sessionVars.GetSessionOrGlobalSystemVar(ctx, "timestamp"): the variable's GetSession
    // hook answers the set value, or the current time.
    let timestamp_str = match &ctx.timestamp {
        Some(ts) => ts.clone(),
        None => return Ok(now),
    };

    let (timestamp, res) =
        tidb_types::convert::str_to_float(&ctx.type_ctx, timestamp_str.as_bytes(), false);
    res?;
    let seconds = timestamp.trunc();
    let fractional_seconds = timestamp - seconds;
    Ok(tidb_base::gostd::time::unix(
        seconds as i64,
        (fractional_seconds * 1e9) as i64,
    ))
}

/// `currentUserProp`: the session's `User` and `ActiveRoles`, which the Rust `SessionVars`
/// does not have (the privilege block D04.1; VS1 plan §3.2: the PoC's single user). The
/// provider is not registered (spec §8.3).
// glue: stub of currentUserProp, blocked-by:D04.1 (SessionVars.User / ActiveRoles)
fn current_user_prop() -> Result<CurrentUserPropProvider, Error> {
    Err(blocked(
        "D04.1",
        "the current user of the session (SessionVars.User / ActiveRoles)",
    ))
}

/// The InfoSchema as `infoschema/context.MetaOnlyInfoSchema` (the stand-in interface has
/// only the schema version).
// glue: infoschema.InfoSchema as a MetaOnlyInfoSchema interface value
struct MetaOnly(InfoSchemaRef);

impl MetaOnlyInfoSchema for MetaOnly {
    // glue: interface method MetaOnlyInfoSchema.SchemaMetaVersion
    fn schema_meta_version(&self) -> i64 {
        self.0.schema_meta_version()
    }
}

// go: infoSchemaProp L1
fn info_schema_prop(sctx: Arc<SessionView>) -> InfoSchemaPropProvider {
    InfoSchemaPropProvider(Arc::new(move |is_domain| {
        if is_domain {
            return Some(
                Arc::new(MetaOnly(Arc::clone(&sctx.domain_info_schema))) as MetaOnlyInfoSchemaRef
            );
        }
        Some(Arc::new(MetaOnly(Arc::clone(&sctx.info_schema))) as MetaOnlyInfoSchemaRef)
    }))
}

/// `sqlExecutorProp`: `sctx.GetRestrictedSQLExecutor()`; the restricted SQL executor
/// (`util/sqlexec`, D00.3) is not ported, so asking for it is a boundary.
// go: sqlExecutorProp L2 (the restricted SQL executor is blocked-by:D00.3)
fn sql_executor_prop() -> SQLExecutorPropProvider {
    SQLExecutorPropProvider(Arc::new(|| -> Result<SQLExecutorRef, Error> {
        // glue: blocked-by:D00.3 (the restricted SQL executor of the session, util/sqlexec)
        Err(blocked(
            "D00.3",
            "the restricted SQL executor of the session (GetRestrictedSQLExecutor)",
        ))
    }))
}

/// `sequenceOperatorProp`: `util.GetSequenceByName(sctx.GetInfoSchema(), db, name)`; the
/// Rust InfoSchema has no sequences (`SequenceTable`, D06.4), so asking for one is a
/// boundary. `sequenceOperator` and its three methods come with it.
// go: sequenceOperatorProp L2 (sequences are blocked-by:D06.4)
fn sequence_operator_prop() -> SequenceOperatorProvider {
    SequenceOperatorProvider(Arc::new(
        |_db: &str, _name: &str| -> Result<SequenceOperatorRef, Error> {
            // The operator (Go's sequenceOperator over util.SequenceTable) is never built:
            // glue: stub of sequenceOperator.GetSequenceID, blocked-by:D06.4 (the sequence table of util.GetSequenceByName)
            // glue: stub of sequenceOperator.GetSequenceNextVal, blocked-by:D06.4 (the sequence table of util.GetSequenceByName)
            // glue: stub of sequenceOperator.SetSequenceVal, blocked-by:D06.4 (the sequence table of util.GetSequenceByName)
            Err(blocked(
                "D06.4",
                "sequences in expressions (util.GetSequenceByName)",
            ))
        },
    ))
}
