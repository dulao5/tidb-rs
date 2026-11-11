//! go-file: pkg/expression/exprstatic/exprctx.go
//!
//! Mapping: `*ExprContext` is `Arc<ExprContext>`; `*contextutil.PlanCacheTracker` is
//! `Arc<PlanCacheTracker>`; `*mathutil.MysqlRng` is `Arc<Mutex<MysqlRng>>`. Options are boxed
//! closures over the state (Go `func(*exprCtxState)`).

// Names keep their Go spelling (mapping rule M-GONAME).
#![allow(non_upper_case_globals)]

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;
use tidb_base::Error;
use tidb_base::contextutil::{PlanCacheTracker, WarnAppenderRef, new_plan_cache_tracker};
use tidb_base::mysql::{DefaultCharset, DefaultCollationName};

use super::evalctx::{
    EvalContext, make_eval_context_static, new_eval_context, new_session_vars_with_system_variables,
};
use crate::exprctx::{
    BuildContext, EvalContextRef, ExprContext as ExprContextTrait, MysqlRngRef,
    PlanColumnIDAllocator, StaticConvertibleEvalContextRef, StaticConvertibleExprContext,
    new_simple_plan_column_id_allocator,
};
use crate::variable::names::{
    CharacterSetConnection, CollationConnection, DefaultCollationForUTF8MB4, GroupConcatMaxLen,
    TiDBEnableNoopFuncs,
};
use crate::variable::session::SessionVars;
use crate::variable::session_state::MysqlRng;
use crate::variable::tidb_defs::{DefBlockEncryptionMode, DefGroupConcatMaxLen, DefSysdateIsNow};
use crate::variable::tidb_vars::DefTiDBEnableNoopFuncs;
use crate::variable::varsutil::tidb_opt_on_off_warn;

/// Go `variable.BlockEncryptionMode`.
const BlockEncryptionMode: &str = "block_encryption_mode";
/// Go `variable.TiDBSysdateIsNow`.
const TiDBSysdateIsNow: &str = "tidb_sysdate_is_now";
/// Go `variable.WindowingUseHighPrecision`.
const WindowingUseHighPrecision: &str = "windowing_use_high_precision";

/// The values an `ExprContext` holds (Go `exprCtxState`).
#[derive(Clone)]
pub struct ExprCtxState {
    eval_ctx: Option<Arc<EvalContext>>,
    charset: String,
    collation: String,
    default_collation_for_utf8mb4: String,
    block_encryption_mode: String,
    sys_date_is_now: bool,
    noop_funcs_mode: i64,
    rng: Option<MysqlRngRef>,
    plan_cache_tracker: Option<Arc<PlanCacheTracker>>,
    column_id_allocator: Option<Arc<dyn PlanColumnIDAllocator>>,
    connection_id: u64,
    windowing_use_high_precision: bool,
    group_concat_max_len: u64,
}

/// ExprCtxOption is the option to set `ExprContext`.
pub type ExprCtxOption = Box<dyn FnOnce(&mut ExprCtxState)>;

/// WithEvalCtx sets the `EvalContext` for `ExprContext`.
// go: WithEvalCtx L1
pub fn with_eval_ctx(ctx: Arc<EvalContext>) -> ExprCtxOption {
    Box::new(move |s| s.eval_ctx = Some(ctx))
}

/// WithCharset sets the charset and collation for `ExprContext`.
// go: WithCharset L1
pub fn with_charset(charset: String, collation: String) -> ExprCtxOption {
    Box::new(move |s| {
        s.charset = charset;
        s.collation = collation;
    })
}

/// WithDefaultCollationForUTF8MB4 sets the default collation for utf8mb4 for `ExprContext`.
// go: WithDefaultCollationForUTF8MB4 L1
pub fn with_default_collation_for_utf8mb4(collation: String) -> ExprCtxOption {
    Box::new(move |s| s.default_collation_for_utf8mb4 = collation)
}

/// WithBlockEncryptionMode sets the block encryption mode for `ExprContext`.
// go: WithBlockEncryptionMode L1
pub fn with_block_encryption_mode(mode: String) -> ExprCtxOption {
    Box::new(move |s| s.block_encryption_mode = mode)
}

/// WithSysDateIsNow sets the sysdate is now for `ExprContext`.
// go: WithSysDateIsNow L1
pub fn with_sys_date_is_now(now: bool) -> ExprCtxOption {
    Box::new(move |s| s.sys_date_is_now = now)
}

/// WithNoopFuncsMode sets the noop funcs mode for `ExprContext`.
// go: WithNoopFuncsMode L1
pub fn with_noop_funcs_mode(mode: i64) -> ExprCtxOption {
    Box::new(move |s| s.noop_funcs_mode = mode)
}

/// WithRng sets the rng for `ExprContext`.
// go: WithRng L1
pub fn with_rng(rng: MysqlRngRef) -> ExprCtxOption {
    Box::new(move |s| s.rng = Some(rng))
}

/// WithPlanCacheTracker sets the plan cache tracker for `ExprContext`.
// go: WithPlanCacheTracker L1
pub fn with_plan_cache_tracker(tracker: Arc<PlanCacheTracker>) -> ExprCtxOption {
    Box::new(move |s| s.plan_cache_tracker = Some(tracker))
}

/// WithColumnIDAllocator sets the column id allocator for `ExprContext`.
// go: WithColumnIDAllocator L1
pub fn with_column_id_allocator(allocator: Arc<dyn PlanColumnIDAllocator>) -> ExprCtxOption {
    Box::new(move |s| s.column_id_allocator = Some(allocator))
}

/// WithConnectionID sets the connection id for `ExprContext`.
// go: WithConnectionID L1
pub fn with_connection_id(id: u64) -> ExprCtxOption {
    Box::new(move |s| s.connection_id = id)
}

/// WithWindowingUseHighPrecision sets the windowing use high precision for `ExprContext`.
// go: WithWindowingUseHighPrecision L1
pub fn with_windowing_use_high_precision(use_high_precision: bool) -> ExprCtxOption {
    Box::new(move |s| s.windowing_use_high_precision = use_high_precision)
}

/// WithGroupConcatMaxLen sets the group concat max len for `ExprContext`.
// go: WithGroupConcatMaxLen L1
pub fn with_group_concat_max_len(max_len: u64) -> ExprCtxOption {
    Box::new(move |s| s.group_concat_max_len = max_len)
}

/// ExprContext implements `exprctx.ExprContext` interface.
pub struct ExprContext {
    state: ExprCtxState,
}

/// NewExprContext creates a new ExprContext
// go: NewExprContext L1
pub fn new_expr_context(opts: Vec<ExprCtxOption>) -> Arc<ExprContext> {
    let (cs, _err) = tidb_parser::gocharset::get_charset_info(DefaultCharset);
    let cs = cs.expect("the default charset exists");
    let mut state = ExprCtxState {
        eval_ctx: None,
        charset: cs.name.clone(),
        collation: cs.default_collation(),
        default_collation_for_utf8mb4: DefaultCollationName.to_owned(),
        block_encryption_mode: DefBlockEncryptionMode.to_owned(),
        sys_date_is_now: DefSysdateIsNow,
        noop_funcs_mode: tidb_opt_on_off_warn(DefTiDBEnableNoopFuncs),
        rng: None,
        plan_cache_tracker: None,
        column_id_allocator: None,
        connection_id: 0,
        windowing_use_high_precision: true,
        group_concat_max_len: DefGroupConcatMaxLen,
    };
    for opt in opts {
        opt(&mut state);
    }

    if state.eval_ctx.is_none() {
        state.eval_ctx = Some(new_eval_context(Vec::new()));
    }

    if state.rng.is_none() {
        state.rng = Some(Arc::new(Mutex::new(MysqlRng::new_with_time())));
    }

    if state.column_id_allocator.is_none() {
        state.column_id_allocator = Some(Arc::new(new_simple_plan_column_id_allocator(0)));
    }

    if state.plan_cache_tracker.is_none() {
        let eval: WarnAppenderRef = Arc::clone(state.eval_ctx.as_ref().expect("set above")) as _;
        let cache_tracker = new_plan_cache_tracker(Some(eval));
        cache_tracker.enable_plan_cache();
        state.plan_cache_tracker = Some(Arc::new(cache_tracker));
    }
    Arc::new(ExprContext { state })
}

impl ExprContext {
    /// Apply returns a new `ExprContext` with the fields updated according to the given
    /// options.
    // go: ExprContext.Apply L1
    pub fn apply(&self, opts: Vec<ExprCtxOption>) -> Arc<ExprContext> {
        let mut state = self.state.clone();
        for opt in opts {
            opt(&mut state);
        }
        Arc::new(ExprContext { state })
    }

    /// GetStaticEvalCtx returns the inner `EvalContext`.
    // go: ExprContext.GetStaticEvalCtx L1
    pub fn get_static_eval_ctx(&self) -> Arc<EvalContext> {
        Arc::clone(self.eval())
    }

    // glue: the `*EvalContext` field (never nil after NewExprContext)
    fn eval(&self) -> &Arc<EvalContext> {
        self.state.eval_ctx.as_ref().expect("set by NewExprContext")
    }

    // glue: the plan cache tracker field (never nil after NewExprContext)
    fn tracker(&self) -> &Arc<PlanCacheTracker> {
        self.state
            .plan_cache_tracker
            .as_ref()
            .expect("set by NewExprContext")
    }

    // glue: the column id allocator field (never nil after NewExprContext)
    fn allocator(&self) -> &Arc<dyn PlanColumnIDAllocator> {
        self.state
            .column_id_allocator
            .as_ref()
            .expect("set by NewExprContext")
    }

    /// LoadSystemVars loads system variables and returns a new `EvalContext` with system
    /// variables loaded.
    // go: ExprContext.LoadSystemVars L1 [M-MAPORDER]
    pub fn load_system_vars(
        &self,
        sys_vars: &HashMap<String, String>,
    ) -> Result<Arc<ExprContext>, Error> {
        let session_vars = new_session_vars_with_system_variables(sys_vars)?;
        Ok(self.load_session_vars_internal(&session_vars, sys_vars))
    }

    // go: ExprContext.loadSessionVarsInternal L1 [M-MAPORDER]
    fn load_session_vars_internal(
        &self,
        session_vars: &SessionVars,
        sys_vars: &HashMap<String, String>,
    ) -> Arc<ExprContext> {
        let mut opts: Vec<ExprCtxOption> = Vec::with_capacity(8);
        opts.push(with_eval_ctx(
            self.eval()
                .load_session_vars_internal(session_vars, sys_vars),
        ));
        for name in sys_vars.keys() {
            let name = tidb_base::gostd::to_lower(name);
            match name.as_str() {
                n if n == CharacterSetConnection || n == CollationConnection => {
                    let (cs, col) = session_vars_charset_info(session_vars);
                    opts.push(with_charset(cs, col));
                }
                n if n == DefaultCollationForUTF8MB4 => {
                    opts.push(with_default_collation_for_utf8mb4(
                        session_vars.default_collation_for_utf8mb4.clone(),
                    ))
                }
                n if n == BlockEncryptionMode => {
                    let (block_mode, ok) = session_vars.get_system_var(BlockEncryptionMode);
                    if ok {
                        opts.push(with_block_encryption_mode(block_mode));
                    }
                }
                n if n == TiDBSysdateIsNow => {
                    opts.push(with_sys_date_is_now(session_vars.sysdate_is_now))
                }
                n if n == TiDBEnableNoopFuncs => {
                    opts.push(with_noop_funcs_mode(session_vars.noop_funcs_mode))
                }
                n if n == WindowingUseHighPrecision => opts.push(
                    with_windowing_use_high_precision(session_vars.windowing_use_high_precision),
                ),
                n if n == GroupConcatMaxLen => {
                    opts.push(with_group_concat_max_len(session_vars.group_concat_max_len))
                }
                _ => {}
            }
        }
        self.apply(opts)
    }
}

/// `SessionVars.GetCharsetInfo` (D05.2, not ported yet): the connection charset and
/// collation.
// go-dep: github.com/pingcap/tidb/pkg/sessionctx/variable.SessionVars.GetCharsetInfo L1
fn session_vars_charset_info(vars: &SessionVars) -> (String, String) {
    (
        vars.get_system_var(CharacterSetConnection).0,
        vars.get_system_var(CollationConnection).0,
    )
}

impl BuildContext for ExprContext {
    /// GetEvalCtx implements the `ExprContext.GetEvalCtx`.
    // go: ExprContext.GetEvalCtx L1
    fn get_eval_ctx(&self) -> EvalContextRef {
        Arc::clone(self.eval()) as EvalContextRef
    }

    /// GetCharsetInfo implements the `ExprContext.GetCharsetInfo`.
    // go: ExprContext.GetCharsetInfo L1
    fn get_charset_info(&self) -> (String, String) {
        (self.state.charset.clone(), self.state.collation.clone())
    }

    /// GetDefaultCollationForUTF8MB4 implements the `ExprContext.GetDefaultCollationForUTF8MB4`.
    // go: ExprContext.GetDefaultCollationForUTF8MB4 L1
    fn get_default_collation_for_utf8mb4(&self) -> String {
        self.state.default_collation_for_utf8mb4.clone()
    }

    /// GetBlockEncryptionMode implements the `ExprContext.GetBlockEncryptionMode`.
    // go: ExprContext.GetBlockEncryptionMode L1
    fn get_block_encryption_mode(&self) -> String {
        self.state.block_encryption_mode.clone()
    }

    /// GetSysdateIsNow implements the `ExprContext.GetSysdateIsNow`.
    // go: ExprContext.GetSysdateIsNow L1
    fn get_sysdate_is_now(&self) -> bool {
        self.state.sys_date_is_now
    }

    /// GetNoopFuncsMode implements the `ExprContext.GetNoopFuncsMode`.
    // go: ExprContext.GetNoopFuncsMode L1
    fn get_noop_funcs_mode(&self) -> i64 {
        self.state.noop_funcs_mode
    }

    /// Rng implements the `ExprContext.Rng`.
    // go: ExprContext.Rng L1
    fn rng(&self) -> MysqlRngRef {
        Arc::clone(self.state.rng.as_ref().expect("set by NewExprContext"))
    }

    /// IsUseCache implements the `ExprContext.IsUseCache`.
    // go: ExprContext.IsUseCache L1
    fn is_use_cache(&self) -> bool {
        self.tracker().use_cache()
    }

    /// SetSkipPlanCache implements the `ExprContext.SetSkipPlanCache`.
    // go: ExprContext.SetSkipPlanCache L1
    fn set_skip_plan_cache(&self, reason: &str) {
        self.tracker().set_skip_plan_cache(reason);
    }

    /// AllocPlanColumnID implements the `ExprContext.AllocPlanColumnID`.
    // go: ExprContext.AllocPlanColumnID L1
    fn alloc_plan_column_id(&self) -> i64 {
        self.allocator().alloc_plan_column_id()
    }

    /// IsInNullRejectCheck implements the `ExprContext.IsInNullRejectCheck` and should
    /// always return false.
    // go: ExprContext.IsInNullRejectCheck L1
    fn is_in_null_reject_check(&self) -> bool {
        false
    }

    /// IsConstantPropagateCheck implements the `ExprContext.IsConstantPropagateCheck` and
    /// should always return false.
    // go: ExprContext.IsConstantPropagateCheck L1
    fn is_constant_propagate_check(&self) -> bool {
        false
    }

    /// ConnectionID implements the `ExprContext.ConnectionID`.
    // go: ExprContext.ConnectionID L1
    fn connection_id(&self) -> u64 {
        self.state.connection_id
    }

    /// IsReadonlyUserVar implements the `BuildContext.IsReadonlyUserVar`.
    /// This method always returns false for simplicity, ensuring the safest behavior across
    /// all scenarios.
    // go: ExprContext.IsReadonlyUserVar L1
    fn is_readonly_user_var(&self, _name: &str) -> bool {
        false
    }
}

impl ExprContextTrait for ExprContext {
    /// GetWindowingUseHighPrecision implements the `ExprContext.GetWindowingUseHighPrecision`.
    // go: ExprContext.GetWindowingUseHighPrecision L1
    fn get_windowing_use_high_precision(&self) -> bool {
        self.state.windowing_use_high_precision
    }

    /// GetGroupConcatMaxLen implements the `ExprContext.GetGroupConcatMaxLen`.
    // go: ExprContext.GetGroupConcatMaxLen L1
    fn get_group_concat_max_len(&self) -> u64 {
        self.state.group_concat_max_len
    }
}

impl StaticConvertibleExprContext for ExprContext {
    /// GetStaticConvertibleEvalContext implements context.StaticConvertibleExprContext.
    // go: ExprContext.GetStaticConvertibleEvalContext L1
    fn get_static_convertible_eval_context(&self) -> StaticConvertibleEvalContextRef {
        Arc::clone(self.eval()) as StaticConvertibleEvalContextRef
    }

    /// GetPlanCacheTracker implements context.StaticConvertibleExprContext.
    // go: ExprContext.GetPlanCacheTracker L1
    fn get_plan_cache_tracker(&self) -> Arc<PlanCacheTracker> {
        Arc::clone(self.tracker())
    }

    /// GetLastPlanColumnID implements context.StaticConvertibleExprContext.
    // go: ExprContext.GetLastPlanColumnID L1
    fn get_last_plan_column_id(&self) -> i64 {
        self.allocator().get_last_plan_column_id()
    }
}

/// MakeExprContextStatic converts the `exprctx.StaticConvertibleExprContext` to
/// `ExprContext`.
// go: MakeExprContextStatic L1
pub fn make_expr_context_static(ctx: &dyn StaticConvertibleExprContext) -> Arc<ExprContext> {
    let static_eval_context = make_eval_context_static(&ctx.get_static_convertible_eval_context());
    let (cs, col) = ctx.get_charset_info();
    new_expr_context(vec![
        with_eval_ctx(static_eval_context),
        with_charset(cs, col),
        with_default_collation_for_utf8mb4(ctx.get_default_collation_for_utf8mb4()),
        with_block_encryption_mode(ctx.get_block_encryption_mode()),
        with_sys_date_is_now(ctx.get_sysdate_is_now()),
        with_noop_funcs_mode(ctx.get_noop_funcs_mode()),
        with_rng(ctx.rng()),
        with_plan_cache_tracker(ctx.get_plan_cache_tracker()),
        with_column_id_allocator(Arc::new(new_simple_plan_column_id_allocator(
            ctx.get_last_plan_column_id(),
        ))),
        with_connection_id(ctx.connection_id()),
        with_windowing_use_high_precision(ctx.get_windowing_use_high_precision()),
        with_group_concat_max_len(ctx.get_group_concat_max_len()),
    ])
}
