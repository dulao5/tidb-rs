//! go-file: pkg/expression/exprctx/context.go
//!
//! Mapping:
//! - `EvalContext`, `BuildContext`, `ExprContext` and the static-convertible variants are
//!   object-safe traits (`Send + Sync`), shared as `Arc<dyn …>` (N4).
//! - Wrapper types that embed an interface in Go (`NullRejectCheckExprContext`,
//!   `innerOverrideEvalContext`, …) hold the inner `Arc` and forward each method.
//! - `*mathutil.MysqlRng` (shared, with a mutex inside in Go) is
//!   `Arc<parking_lot::Mutex<MysqlRng>>`.
//! - `intest.Assert` is compiled out of production Go builds: `debug_assert!` here.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use parking_lot::Mutex;
use tidb_base::Error;
use tidb_base::contextutil::{PlanCacheTracker, SQLWarn, WarnAppender, WarnHandler};
use tidb_base::gostd::time::{Location, Time};
use tidb_base::mysql::SQLMode;
use tidb_types::context::Context as TypeContext;
use tidb_types::datum::Datum;

use super::optional::{OptionalEvalPropKey, OptionalEvalPropKeySet, OptionalEvalPropProviderRef};
use super::param::ParamValues;
use crate::errctx;
use crate::variable::session::SessionVars;
use crate::variable::session_state::MysqlRng;
use crate::variable::uservars::UserVarsReaderRef;

/// A shared `*mathutil.MysqlRng`.
pub type MysqlRngRef = Arc<Mutex<MysqlRng>>;

/// PlanColumnIDAllocator allocates column id for plan.
pub trait PlanColumnIDAllocator: Send + Sync {
    /// AllocPlanColumnID allocates column id for plan.
    // glue: interface method of exprctx.PlanColumnIDAllocator
    fn alloc_plan_column_id(&self) -> i64;
    /// GetLastPlanColumnID returns the last column id.
    // glue: interface method of exprctx.PlanColumnIDAllocator
    fn get_last_plan_column_id(&self) -> i64;
}

/// SimplePlanColumnIDAllocator implements PlanColumnIDAllocator
#[derive(Debug, Default)]
pub struct SimplePlanColumnIDAllocator {
    id: AtomicI64,
}

/// NewSimplePlanColumnIDAllocator creates a new SimplePlanColumnIDAllocator.
// go: NewSimplePlanColumnIDAllocator L1
pub fn new_simple_plan_column_id_allocator(offset: i64) -> SimplePlanColumnIDAllocator {
    let alloc = SimplePlanColumnIDAllocator::default();
    alloc.id.store(offset, Ordering::SeqCst);
    alloc
}

impl PlanColumnIDAllocator for SimplePlanColumnIDAllocator {
    /// AllocPlanColumnID allocates column id for plan.
    // go: SimplePlanColumnIDAllocator.AllocPlanColumnID L1
    fn alloc_plan_column_id(&self) -> i64 {
        // Go atomic.Int64.Add wraps on overflow.
        self.id.fetch_add(1, Ordering::SeqCst).wrapping_add(1)
    }

    /// GetLastPlanColumnID returns the last column id.
    // go: SimplePlanColumnIDAllocator.GetLastPlanColumnID L1
    fn get_last_plan_column_id(&self) -> i64 {
        self.id.load(Ordering::SeqCst)
    }
}

/// EvalContext is used to evaluate an expression
pub trait EvalContext: WarnHandler + ParamValues + Send + Sync {
    /// CtxID indicates the id of the context.
    // glue: interface method of exprctx.EvalContext
    fn ctx_id(&self) -> u64;
    /// SQLMode returns the sql mode
    // glue: interface method of exprctx.EvalContext
    fn sql_mode(&self) -> SQLMode;
    /// TypeCtx returns the types.Context
    // glue: interface method of exprctx.EvalContext
    fn type_ctx(&self) -> TypeContext;
    /// ErrCtx returns the errctx.Context
    // glue: interface method of exprctx.EvalContext
    fn err_ctx(&self) -> errctx::Context;
    /// Location returns the timezone info
    // glue: interface method of exprctx.EvalContext
    fn location(&self) -> Arc<Location>;
    /// CurrentDB return the current database name
    // glue: interface method of exprctx.EvalContext
    fn current_db(&self) -> String;
    /// CurrentTime returns the current time.
    // glue: interface method of exprctx.EvalContext
    fn current_time(&self) -> Result<Time, Error>;
    /// GetMaxAllowedPacket returns the value of the 'max_allowed_packet' system variable.
    // glue: interface method of exprctx.EvalContext
    fn get_max_allowed_packet(&self) -> u64;
    /// GetTiDBRedactLog returns the value of the 'tidb_redact_log' system variable.
    // glue: interface method of exprctx.EvalContext
    fn get_tidb_redact_log(&self) -> String;
    /// GetDefaultWeekFormatMode returns the value of the 'default_week_format' system variable.
    // glue: interface method of exprctx.EvalContext
    fn get_default_week_format_mode(&self) -> String;
    /// GetDivPrecisionIncrement returns the specified value of DivPrecisionIncrement.
    // glue: interface method of exprctx.EvalContext
    fn get_div_precision_increment(&self) -> i64;
    /// GetUserVarsReader returns the `UserVarsReader` to read user vars.
    // glue: interface method of exprctx.EvalContext
    fn get_user_vars_reader(&self) -> UserVarsReaderRef;
    /// GetOptionalPropSet returns the optional properties provided by this context.
    // glue: interface method of exprctx.EvalContext
    fn get_optional_prop_set(&self) -> OptionalEvalPropKeySet;
    /// GetOptionalPropProvider gets the optional property provider by key
    // glue: interface method of exprctx.EvalContext
    fn get_optional_prop_provider(
        &self,
        key: OptionalEvalPropKey,
    ) -> Option<OptionalEvalPropProviderRef>;
}

/// A shared `exprctx.EvalContext` interface value.
pub type EvalContextRef = Arc<dyn EvalContext>;

/// BuildContext is used to build an expression
pub trait BuildContext: Send + Sync {
    /// GetEvalCtx returns the EvalContext.
    // glue: interface method of exprctx.BuildContext
    fn get_eval_ctx(&self) -> EvalContextRef;
    /// GetCharsetInfo returns charset and collation for the current context.
    // glue: interface method of exprctx.BuildContext
    fn get_charset_info(&self) -> (String, String);
    /// GetDefaultCollationForUTF8MB4 returns the default collation of UTF8MB4.
    // glue: interface method of exprctx.BuildContext
    fn get_default_collation_for_utf8mb4(&self) -> String;
    /// GetBlockEncryptionMode returns the variable `block_encryption_mode`.
    // glue: interface method of exprctx.BuildContext
    fn get_block_encryption_mode(&self) -> String;
    /// GetSysdateIsNow returns a bool to determine whether Sysdate is an alias of Now
    /// function. It is the value of variable `tidb_sysdate_is_now`.
    // glue: interface method of exprctx.BuildContext
    fn get_sysdate_is_now(&self) -> bool;
    /// GetNoopFuncsMode returns the noop function mode: OFF/ON/WARN values as 0/1/2.
    // glue: interface method of exprctx.BuildContext
    fn get_noop_funcs_mode(&self) -> i64;
    /// Rng is used to generate random values.
    // glue: interface method of exprctx.BuildContext
    fn rng(&self) -> MysqlRngRef;
    /// IsUseCache indicates whether to cache the build expression in plan cache.
    // glue: interface method of exprctx.BuildContext
    fn is_use_cache(&self) -> bool;
    /// SetSkipPlanCache sets to skip the plan cache and records the reason.
    // glue: interface method of exprctx.BuildContext
    fn set_skip_plan_cache(&self, reason: &str);
    /// AllocPlanColumnID allocates column id for plan.
    // glue: interface method of exprctx.BuildContext
    fn alloc_plan_column_id(&self) -> i64;
    /// IsInNullRejectCheck returns the flag to indicate whether the expression is in null
    /// reject check.
    // glue: interface method of exprctx.BuildContext
    fn is_in_null_reject_check(&self) -> bool;
    /// IsConstantPropagateCheck returns the flag to indicate whether the expression is in
    /// constant propagate check.
    // glue: interface method of exprctx.BuildContext
    fn is_constant_propagate_check(&self) -> bool;
    /// ConnectionID indicates the connection ID of the current session.
    // glue: interface method of exprctx.BuildContext
    fn connection_id(&self) -> u64;
    /// IsReadonlyUserVar checks whether the user variable is readonly.
    // glue: interface method of exprctx.BuildContext
    fn is_readonly_user_var(&self, name: &str) -> bool;
}

/// A shared `exprctx.BuildContext` interface value.
pub type BuildContextRef = Arc<dyn BuildContext>;

/// ExprContext contains full context for expression building and evaluating.
pub trait ExprContext: BuildContext {
    /// GetWindowingUseHighPrecision returns whether the windowing functions use high precision.
    // glue: interface method of exprctx.ExprContext
    fn get_windowing_use_high_precision(&self) -> bool;
    /// GetGroupConcatMaxLen returns the value of the 'group_concat_max_len' system variable.
    // glue: interface method of exprctx.ExprContext
    fn get_group_concat_max_len(&self) -> u64;
}

/// A shared `exprctx.ExprContext` interface value.
pub type ExprContextRef = Arc<dyn ExprContext>;

/// NullRejectCheckExprContext is a wrapper to return true for `IsInNullRejectCheck`.
pub struct NullRejectCheckExprContext {
    expr_ctx: ExprContextRef,
}

/// WithNullRejectCheck returns a new `NullRejectCheckExprContext` with the given
/// `ExprContext`.
// go: WithNullRejectCheck L1
pub fn with_null_reject_check(ctx: ExprContextRef) -> NullRejectCheckExprContext {
    NullRejectCheckExprContext { expr_ctx: ctx }
}

impl BuildContext for NullRejectCheckExprContext {
    // glue: forwarded BuildContext.GetEvalCtx (Go struct embedding)
    fn get_eval_ctx(&self) -> EvalContextRef {
        self.expr_ctx.get_eval_ctx()
    }
    // glue: forwarded BuildContext.GetCharsetInfo (Go struct embedding)
    fn get_charset_info(&self) -> (String, String) {
        self.expr_ctx.get_charset_info()
    }
    // glue: forwarded BuildContext.GetDefaultCollationForUTF8MB4 (Go struct embedding)
    fn get_default_collation_for_utf8mb4(&self) -> String {
        self.expr_ctx.get_default_collation_for_utf8mb4()
    }
    // glue: forwarded BuildContext.GetBlockEncryptionMode (Go struct embedding)
    fn get_block_encryption_mode(&self) -> String {
        self.expr_ctx.get_block_encryption_mode()
    }
    // glue: forwarded BuildContext.GetSysdateIsNow (Go struct embedding)
    fn get_sysdate_is_now(&self) -> bool {
        self.expr_ctx.get_sysdate_is_now()
    }
    // glue: forwarded BuildContext.GetNoopFuncsMode (Go struct embedding)
    fn get_noop_funcs_mode(&self) -> i64 {
        self.expr_ctx.get_noop_funcs_mode()
    }
    // glue: forwarded BuildContext.Rng (Go struct embedding)
    fn rng(&self) -> MysqlRngRef {
        self.expr_ctx.rng()
    }
    // glue: forwarded BuildContext.IsUseCache (Go struct embedding)
    fn is_use_cache(&self) -> bool {
        self.expr_ctx.is_use_cache()
    }
    // glue: forwarded BuildContext.SetSkipPlanCache (Go struct embedding)
    fn set_skip_plan_cache(&self, reason: &str) {
        self.expr_ctx.set_skip_plan_cache(reason)
    }
    // glue: forwarded BuildContext.AllocPlanColumnID (Go struct embedding)
    fn alloc_plan_column_id(&self) -> i64 {
        self.expr_ctx.alloc_plan_column_id()
    }
    /// IsInNullRejectCheck always returns true for `NullRejectCheckExprContext`
    // go: NullRejectCheckExprContext.IsInNullRejectCheck L1
    fn is_in_null_reject_check(&self) -> bool {
        true
    }
    // glue: forwarded BuildContext.IsConstantPropagateCheck (Go struct embedding)
    fn is_constant_propagate_check(&self) -> bool {
        self.expr_ctx.is_constant_propagate_check()
    }
    // glue: forwarded BuildContext.ConnectionID (Go struct embedding)
    fn connection_id(&self) -> u64 {
        self.expr_ctx.connection_id()
    }
    // glue: forwarded BuildContext.IsReadonlyUserVar (Go struct embedding)
    fn is_readonly_user_var(&self, name: &str) -> bool {
        self.expr_ctx.is_readonly_user_var(name)
    }
}

impl ExprContext for NullRejectCheckExprContext {
    // glue: forwarded ExprContext.GetWindowingUseHighPrecision (Go struct embedding)
    fn get_windowing_use_high_precision(&self) -> bool {
        self.expr_ctx.get_windowing_use_high_precision()
    }
    // glue: forwarded ExprContext.GetGroupConcatMaxLen (Go struct embedding)
    fn get_group_concat_max_len(&self) -> u64 {
        self.expr_ctx.get_group_concat_max_len()
    }
}

/// ConstantPropagateCheckContext is a wrapper to return true for `IsConstantPropagateCheck`.
pub struct ConstantPropagateCheckContext {
    expr_ctx: ExprContextRef,
}

/// WithConstantPropagateCheck returns a new `ConstantPropagateCheckContext` with the given
/// `ExprContext`.
// go: WithConstantPropagateCheck L1
pub fn with_constant_propagate_check(ctx: ExprContextRef) -> ConstantPropagateCheckContext {
    ConstantPropagateCheckContext { expr_ctx: ctx }
}

impl BuildContext for ConstantPropagateCheckContext {
    // glue: forwarded BuildContext.GetEvalCtx (Go struct embedding)
    fn get_eval_ctx(&self) -> EvalContextRef {
        self.expr_ctx.get_eval_ctx()
    }
    // glue: forwarded BuildContext.GetCharsetInfo (Go struct embedding)
    fn get_charset_info(&self) -> (String, String) {
        self.expr_ctx.get_charset_info()
    }
    // glue: forwarded BuildContext.GetDefaultCollationForUTF8MB4 (Go struct embedding)
    fn get_default_collation_for_utf8mb4(&self) -> String {
        self.expr_ctx.get_default_collation_for_utf8mb4()
    }
    // glue: forwarded BuildContext.GetBlockEncryptionMode (Go struct embedding)
    fn get_block_encryption_mode(&self) -> String {
        self.expr_ctx.get_block_encryption_mode()
    }
    // glue: forwarded BuildContext.GetSysdateIsNow (Go struct embedding)
    fn get_sysdate_is_now(&self) -> bool {
        self.expr_ctx.get_sysdate_is_now()
    }
    // glue: forwarded BuildContext.GetNoopFuncsMode (Go struct embedding)
    fn get_noop_funcs_mode(&self) -> i64 {
        self.expr_ctx.get_noop_funcs_mode()
    }
    // glue: forwarded BuildContext.Rng (Go struct embedding)
    fn rng(&self) -> MysqlRngRef {
        self.expr_ctx.rng()
    }
    // glue: forwarded BuildContext.IsUseCache (Go struct embedding)
    fn is_use_cache(&self) -> bool {
        self.expr_ctx.is_use_cache()
    }
    // glue: forwarded BuildContext.SetSkipPlanCache (Go struct embedding)
    fn set_skip_plan_cache(&self, reason: &str) {
        self.expr_ctx.set_skip_plan_cache(reason)
    }
    // glue: forwarded BuildContext.AllocPlanColumnID (Go struct embedding)
    fn alloc_plan_column_id(&self) -> i64 {
        self.expr_ctx.alloc_plan_column_id()
    }
    // glue: forwarded BuildContext.IsInNullRejectCheck (Go struct embedding)
    fn is_in_null_reject_check(&self) -> bool {
        self.expr_ctx.is_in_null_reject_check()
    }
    /// IsConstantPropagateCheck always returns true for `ConstantPropagateCheckContext`
    // go: ConstantPropagateCheckContext.IsConstantPropagateCheck L1
    fn is_constant_propagate_check(&self) -> bool {
        true
    }
    // glue: forwarded BuildContext.ConnectionID (Go struct embedding)
    fn connection_id(&self) -> u64 {
        self.expr_ctx.connection_id()
    }
    // glue: forwarded BuildContext.IsReadonlyUserVar (Go struct embedding)
    fn is_readonly_user_var(&self, name: &str) -> bool {
        self.expr_ctx.is_readonly_user_var(name)
    }
}

impl ExprContext for ConstantPropagateCheckContext {
    // glue: forwarded ExprContext.GetWindowingUseHighPrecision (Go struct embedding)
    fn get_windowing_use_high_precision(&self) -> bool {
        self.expr_ctx.get_windowing_use_high_precision()
    }
    // glue: forwarded ExprContext.GetGroupConcatMaxLen (Go struct embedding)
    fn get_group_concat_max_len(&self) -> u64 {
        self.expr_ctx.get_group_concat_max_len()
    }
}

/// innerOverrideEvalContext is used to override the type context and error context of an
/// EvalContext.
struct InnerOverrideEvalContext {
    eval_ctx: EvalContextRef,
    type_ctx: TypeContext,
    err_ctx: errctx::Context,
}

impl WarnAppender for InnerOverrideEvalContext {
    // glue: forwarded EvalContext.AppendWarning (Go struct embedding)
    fn append_warning(&self, err: Error) {
        self.eval_ctx.append_warning(err)
    }
    // glue: forwarded EvalContext.AppendNote (Go struct embedding)
    fn append_note(&self, msg: Error) {
        self.eval_ctx.append_note(msg)
    }
}

impl WarnHandler for InnerOverrideEvalContext {
    // glue: forwarded EvalContext.WarningCount (Go struct embedding)
    fn warning_count(&self) -> i64 {
        self.eval_ctx.warning_count()
    }
    // glue: forwarded EvalContext.TruncateWarnings (Go struct embedding)
    fn truncate_warnings(&self, start: i64) -> Vec<SQLWarn> {
        self.eval_ctx.truncate_warnings(start)
    }
    // glue: forwarded EvalContext.CopyWarnings (Go struct embedding)
    fn copy_warnings(&self, dst: Vec<SQLWarn>) -> Vec<SQLWarn> {
        self.eval_ctx.copy_warnings(dst)
    }
}

impl ParamValues for InnerOverrideEvalContext {
    // glue: forwarded EvalContext.GetParamValue (Go struct embedding)
    fn get_param_value(&self, idx: i64) -> Result<Datum, Error> {
        self.eval_ctx.get_param_value(idx)
    }
}

impl EvalContext for InnerOverrideEvalContext {
    // glue: forwarded EvalContext.CtxID (Go struct embedding)
    fn ctx_id(&self) -> u64 {
        self.eval_ctx.ctx_id()
    }
    // glue: forwarded EvalContext.SQLMode (Go struct embedding)
    fn sql_mode(&self) -> SQLMode {
        self.eval_ctx.sql_mode()
    }
    /// TypeCtx implements EvalContext.TypeCtx
    // go: innerOverrideEvalContext.TypeCtx L1
    fn type_ctx(&self) -> TypeContext {
        self.type_ctx.clone()
    }
    /// ErrCtx implements EvalContext.GetEvalCtx
    // go: innerOverrideEvalContext.ErrCtx L1
    fn err_ctx(&self) -> errctx::Context {
        self.err_ctx.clone()
    }
    // glue: forwarded EvalContext.Location (Go struct embedding)
    fn location(&self) -> Arc<Location> {
        self.eval_ctx.location()
    }
    // glue: forwarded EvalContext.CurrentDB (Go struct embedding)
    fn current_db(&self) -> String {
        self.eval_ctx.current_db()
    }
    // glue: forwarded EvalContext.CurrentTime (Go struct embedding)
    fn current_time(&self) -> Result<Time, Error> {
        self.eval_ctx.current_time()
    }
    // glue: forwarded EvalContext.GetMaxAllowedPacket (Go struct embedding)
    fn get_max_allowed_packet(&self) -> u64 {
        self.eval_ctx.get_max_allowed_packet()
    }
    // glue: forwarded EvalContext.GetTiDBRedactLog (Go struct embedding)
    fn get_tidb_redact_log(&self) -> String {
        self.eval_ctx.get_tidb_redact_log()
    }
    // glue: forwarded EvalContext.GetDefaultWeekFormatMode (Go struct embedding)
    fn get_default_week_format_mode(&self) -> String {
        self.eval_ctx.get_default_week_format_mode()
    }
    // glue: forwarded EvalContext.GetDivPrecisionIncrement (Go struct embedding)
    fn get_div_precision_increment(&self) -> i64 {
        self.eval_ctx.get_div_precision_increment()
    }
    // glue: forwarded EvalContext.GetUserVarsReader (Go struct embedding)
    fn get_user_vars_reader(&self) -> UserVarsReaderRef {
        self.eval_ctx.get_user_vars_reader()
    }
    // glue: forwarded EvalContext.GetOptionalPropSet (Go struct embedding)
    fn get_optional_prop_set(&self) -> OptionalEvalPropKeySet {
        self.eval_ctx.get_optional_prop_set()
    }
    // glue: forwarded EvalContext.GetOptionalPropProvider (Go struct embedding)
    fn get_optional_prop_provider(
        &self,
        key: OptionalEvalPropKey,
    ) -> Option<OptionalEvalPropProviderRef> {
        self.eval_ctx.get_optional_prop_provider(key)
    }
}

/// innerOverrideBuildContext is used to override the eval context of a BuildContext.
struct InnerOverrideBuildContext {
    build_ctx: BuildContextRef,
    eval_ctx: EvalContextRef,
}

impl BuildContext for InnerOverrideBuildContext {
    /// GetEvalCtx implements BuildContext.GetEvalCtx
    // go: innerOverrideBuildContext.GetEvalCtx L1
    fn get_eval_ctx(&self) -> EvalContextRef {
        Arc::clone(&self.eval_ctx)
    }
    // glue: forwarded BuildContext.GetCharsetInfo (Go struct embedding)
    fn get_charset_info(&self) -> (String, String) {
        self.build_ctx.get_charset_info()
    }
    // glue: forwarded BuildContext.GetDefaultCollationForUTF8MB4 (Go struct embedding)
    fn get_default_collation_for_utf8mb4(&self) -> String {
        self.build_ctx.get_default_collation_for_utf8mb4()
    }
    // glue: forwarded BuildContext.GetBlockEncryptionMode (Go struct embedding)
    fn get_block_encryption_mode(&self) -> String {
        self.build_ctx.get_block_encryption_mode()
    }
    // glue: forwarded BuildContext.GetSysdateIsNow (Go struct embedding)
    fn get_sysdate_is_now(&self) -> bool {
        self.build_ctx.get_sysdate_is_now()
    }
    // glue: forwarded BuildContext.GetNoopFuncsMode (Go struct embedding)
    fn get_noop_funcs_mode(&self) -> i64 {
        self.build_ctx.get_noop_funcs_mode()
    }
    // glue: forwarded BuildContext.Rng (Go struct embedding)
    fn rng(&self) -> MysqlRngRef {
        self.build_ctx.rng()
    }
    // glue: forwarded BuildContext.IsUseCache (Go struct embedding)
    fn is_use_cache(&self) -> bool {
        self.build_ctx.is_use_cache()
    }
    // glue: forwarded BuildContext.SetSkipPlanCache (Go struct embedding)
    fn set_skip_plan_cache(&self, reason: &str) {
        self.build_ctx.set_skip_plan_cache(reason)
    }
    // glue: forwarded BuildContext.AllocPlanColumnID (Go struct embedding)
    fn alloc_plan_column_id(&self) -> i64 {
        self.build_ctx.alloc_plan_column_id()
    }
    // glue: forwarded BuildContext.IsInNullRejectCheck (Go struct embedding)
    fn is_in_null_reject_check(&self) -> bool {
        self.build_ctx.is_in_null_reject_check()
    }
    // glue: forwarded BuildContext.IsConstantPropagateCheck (Go struct embedding)
    fn is_constant_propagate_check(&self) -> bool {
        self.build_ctx.is_constant_propagate_check()
    }
    // glue: forwarded BuildContext.ConnectionID (Go struct embedding)
    fn connection_id(&self) -> u64 {
        self.build_ctx.connection_id()
    }
    // glue: forwarded BuildContext.IsReadonlyUserVar (Go struct embedding)
    fn is_readonly_user_var(&self, name: &str) -> bool {
        self.build_ctx.is_readonly_user_var(name)
    }
}

/// CtxWithHandleTruncateErrLevel returns a new BuildContext with the specified level for
/// handling truncate error.
///
/// The returned context is the given one (the same `Arc`) when nothing changes, as Go returns
/// the same interface value.
// go: CtxWithHandleTruncateErrLevel L1
pub fn ctx_with_handle_truncate_err_level(
    ctx: BuildContextRef,
    level: errctx::Level,
) -> BuildContextRef {
    let (mut truncate_as_warnings, mut ignore_truncate) = (false, false);
    match level {
        errctx::LevelWarn => truncate_as_warnings = true,
        errctx::LevelIgnore => ignore_truncate = true,
        _ => {}
    }

    let eval_ctx = ctx.get_eval_ctx();
    let (tc, ec) = (eval_ctx.type_ctx(), eval_ctx.err_ctx());

    let flags = tc
        .flags()
        .with_truncate_as_warning(truncate_as_warnings)
        .with_ignore_truncate_err(ignore_truncate);

    if tc.flags() == flags && ec.level_for_group(errctx::ErrGroupTruncate) == level {
        return ctx;
    }

    let inner_eval: EvalContextRef = Arc::new(InnerOverrideEvalContext {
        eval_ctx,
        type_ctx: tc.with_flags(flags),
        err_ctx: ec.with_err_group_level(errctx::ErrGroupTruncate, level),
    });
    Arc::new(InnerOverrideBuildContext {
        build_ctx: ctx,
        eval_ctx: inner_eval,
    })
}

/// AssertLocationWithSessionVars asserts the location in the context and session variables
/// are the same. It is only used for testing.
///
/// `intest.Assert` is a no-op in production Go builds: a `debug_assert!` here. The D05.2
/// stand-in `StatementContext` has no time zone yet, so the statement location is not
/// compared (the session's and the context's are).
// go: AssertLocationWithSessionVars L2 (stmt ctx time zone not compared until D05.2 T9)
pub fn assert_location_with_session_vars(ctx_loc: &Arc<Location>, vars: &SessionVars) {
    let ctx_loc_str = ctx_loc.string().to_owned();
    let vars_loc_str = vars.location().string().to_owned();
    debug_assert!(
        ctx_loc_str == vars_loc_str,
        "location mismatch, ctxLoc: {ctx_loc_str}, varsLoc: {vars_loc_str}",
    );
}

/// StaticConvertibleExprContext provides more methods to implement the clone of a
/// `ExprContext`
pub trait StaticConvertibleExprContext: ExprContext {
    /// GetStaticConvertibleEvalContext returns the eval context, convertible to a static one.
    // glue: interface method of exprctx.StaticConvertibleExprContext
    fn get_static_convertible_eval_context(&self) -> StaticConvertibleEvalContextRef;
    /// GetPlanCacheTracker returns the plan cache tracker.
    // glue: interface method of exprctx.StaticConvertibleExprContext
    fn get_plan_cache_tracker(&self) -> Arc<PlanCacheTracker>;
    /// GetLastPlanColumnID returns the last plan column id.
    // glue: interface method of exprctx.StaticConvertibleExprContext
    fn get_last_plan_column_id(&self) -> i64;
}

/// StaticConvertibleEvalContext provides more methods to implement the clone of a
/// `EvalContext`
pub trait StaticConvertibleEvalContext: EvalContext {
    /// AllParamValues returns all the parameter values.
    // glue: interface method of exprctx.StaticConvertibleEvalContext
    fn all_param_values(&self) -> Vec<Datum>;
    /// GetWarnHandler returns the warn handler.
    // glue: interface method of exprctx.StaticConvertibleEvalContext
    fn get_warn_handler(&self) -> WarnHandlerRef;
}

/// A shared `exprctx.StaticConvertibleEvalContext` interface value.
pub type StaticConvertibleEvalContextRef = Arc<dyn StaticConvertibleEvalContext>;

/// A shared `contextutil.WarnHandler` interface value.
pub type WarnHandlerRef = Arc<dyn WarnHandler + Send + Sync>;
