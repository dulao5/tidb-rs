//! go-file: pkg/expression/exprstatic/evalctx.go
//!
//! Mapping:
//! - `*EvalContext` is `Arc<EvalContext>`; `Apply` takes `self: &Arc<Self>` because the new
//!   context's current time reads the old context (Go captures `ctx.CurrentTime`).
//! - Go passes the context itself as the `WarnAppender` of its `types.Context` and
//!   `errctx.Context`, so the options that replace `warnHandler` after those are built are
//!   still seen. Here they hold a shared [`EvalWarnSink`] that forwards to the context's
//!   handler, set once the options are applied (the context is immutable afterwards).
//! - `variable.PlanCacheParamList` (D05.2) is not ported: `WithParamList` takes the parameter
//!   values.
//! - `intest.Assert*` are compiled out of production Go builds and are not ported.
//! - Go `map[string]string` iteration order is random; `LoadSystemVars` takes a `HashMap`
//!   (M-MAPORDER): the result does not depend on the order unless two variables fail.

// Names keep their Go spelling (mapping rule M-GONAME).
#![allow(non_upper_case_globals)]

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, OnceLock};

use parking_lot::Mutex;
use tidb_base::Error;
use tidb_base::contextutil::{
    SQLWarn, WarnAppender, WarnAppenderRef, WarnHandler, gen_context_id, new_static_warn_handler,
};
use tidb_base::gostd::conv::f64_to_i64;
use tidb_base::gostd::time::{Location, Time, now, unix, utc};
use tidb_base::mysql::{DefaultSQLMode, SQLMode, get_sql_mode};
use tidb_types::context::{Context as TypeContext, Flags, StrictContext, StrictFlags, new_context};
use tidb_types::datum::Datum;

use crate::errctx;
use crate::exprctx::{
    EvalContext as EvalContextTrait, OptionalEvalPropKey, OptionalEvalPropKeySet,
    OptionalEvalPropProviderRef, ParamValues, StaticConvertibleEvalContext,
    StaticConvertibleEvalContextRef, UserVarsReaderRef, WarnHandlerRef,
    err_param_index_exceed_param_counts, new_user_vars,
};
use crate::expropt::OptionalEvalPropProviders;
use crate::variable::names::{
    CharacterSetConnection, CollationConnection, MaxAllowedPacket, SQLModeVar, TimeZone, Timestamp,
};
use crate::variable::session::GlobalVarAccessor;
use crate::variable::session::SessionVars;
use crate::variable::tidb_defs::{
    DefDefaultWeekFormat, DefDivPrecisionIncrement, DefMaxAllowedPacket, DefTiDBRedactLog,
    DefTimestamp, DefaultWeekFormat,
};

/// Go `variable.TiDBRedactLog`.
const TiDBRedactLog: &str = "tidb_redact_log";
/// Go `variable.DivPrecisionIncrement`.
const DivPrecisionIncrement: &str = "div_precision_increment";

/// The function a `timeOnce` calls to get the time (Go `func() (time.Time, error)`).
pub type TimeFn = Arc<dyn Fn() -> Result<Time, Error> + Send + Sync>;

/// timeOnce is used to get the current time once and cache it.
struct TimeOnce {
    time: Mutex<Option<Time>>,
    time_fn: Option<TimeFn>,
}

impl TimeOnce {
    // go: timeOnce.getTime L1
    fn get_time(&self, loc: &Arc<Location>) -> Result<Time, Error> {
        let mut t = self.time.lock();
        if let Some(tm) = &*t {
            return Ok(tm.clone());
        }
        let tm = match &self.time_fn {
            Some(f) => f()?,
            None => now(),
        };
        let tm = tm.in_loc(loc);
        *t = Some(tm.clone());
        Ok(tm)
    }
}

/// The values an `EvalContext` holds (Go `evalCtxState`).
#[derive(Clone)]
pub struct EvalCtxState {
    warn_handler: Option<WarnHandlerRef>,
    sql_mode: SQLMode,
    type_ctx: TypeContext,
    err_ctx: errctx::Context,
    current_db: String,
    current_time: Arc<TimeOnce>,
    max_allowed_packet: u64,
    enable_redact_log: String,
    default_week_format_mode: String,
    div_precision_increment: i64,
    param_list: Vec<Datum>,
    user_vars: Option<UserVarsReaderRef>,
    props: OptionalEvalPropProviders,
}

/// EvalCtxOption is the option to set `EvalContext`
pub type EvalCtxOption = Box<dyn FnOnce(&mut EvalCtxState)>;

/// WithWarnHandler sets the warn handler for the `EvalContext`.
// go: WithWarnHandler L1
pub fn with_warn_handler(h: Option<WarnHandlerRef>) -> EvalCtxOption {
    let h: WarnHandlerRef = match h {
        Some(h) => h,
        None => Arc::new(tidb_base::contextutil::IgnoreWarnHandler),
    };
    Box::new(move |s| s.warn_handler = Some(h))
}

/// WithSQLMode sets the sql mode for the `EvalContext`.
// go: WithSQLMode L1
pub fn with_sql_mode(sql_mode: SQLMode) -> EvalCtxOption {
    Box::new(move |s| s.sql_mode = sql_mode)
}

/// WithTypeFlags sets the type flags for the `EvalContext`.
// go: WithTypeFlags L1
pub fn with_type_flags(flags: Flags) -> EvalCtxOption {
    Box::new(move |s| s.type_ctx = s.type_ctx.with_flags(flags))
}

/// WithLocation sets the timezone info for the `EvalContext`.
// go: WithLocation L1
pub fn with_location(loc: Option<Arc<Location>>) -> EvalCtxOption {
    let loc = loc.unwrap_or_else(utc);
    Box::new(move |s| s.type_ctx = s.type_ctx.with_location(loc))
}

/// WithErrLevelMap sets the error level map for the `EvalContext`.
// go: WithErrLevelMap L1
pub fn with_err_level_map(level: errctx::LevelMap) -> EvalCtxOption {
    Box::new(move |s| s.err_ctx = s.err_ctx.with_err_group_levels(level))
}

/// WithCurrentDB sets the current database name for the `EvalContext`.
// go: WithCurrentDB L1
pub fn with_current_db(db: String) -> EvalCtxOption {
    Box::new(move |s| s.current_db = db)
}

/// WithCurrentTime sets the current time for the `EvalContext`.
// go: WithCurrentTime L1
pub fn with_current_time(f: TimeFn) -> EvalCtxOption {
    Box::new(move |s| {
        s.current_time = Arc::new(TimeOnce {
            time: Mutex::new(None),
            time_fn: Some(f),
        })
    })
}

/// WithMaxAllowedPacket sets the value of the 'max_allowed_packet' system variable.
// go: WithMaxAllowedPacket L1
pub fn with_max_allowed_packet(size: u64) -> EvalCtxOption {
    Box::new(move |s| s.max_allowed_packet = size)
}

/// WithDefaultWeekFormatMode sets the value of the 'default_week_format' system variable.
// go: WithDefaultWeekFormatMode L1
pub fn with_default_week_format_mode(mode: String) -> EvalCtxOption {
    Box::new(move |s| s.default_week_format_mode = mode)
}

/// WithDivPrecisionIncrement sets the value of the 'div_precision_increment' system variable.
// go: WithDivPrecisionIncrement L1
pub fn with_div_precision_increment(inc: i64) -> EvalCtxOption {
    Box::new(move |s| s.div_precision_increment = inc)
}

/// WithOptionalProperty sets the optional property providers
// go: WithOptionalProperty L1
pub fn with_optional_property(providers: Vec<OptionalEvalPropProviderRef>) -> EvalCtxOption {
    Box::new(move |s| {
        s.props = OptionalEvalPropProviders::default();
        for p in providers {
            s.props.add(p);
        }
    })
}

/// WithParamList sets the param list for the `EvalContext`.
///
/// Go takes a `*variable.PlanCacheParamList` (D05.2, not ported yet) and copies its values;
/// the values are passed here.
// go: WithParamList L2 (PlanCacheParamList waits D05.2: takes the values)
pub fn with_param_list(params: &[Datum]) -> EvalCtxOption {
    let list = params.to_vec();
    Box::new(move |s| s.param_list = list)
}

/// WithEnableRedactLog sets the value of the 'tidb_redact_log' system variable.
// go: WithEnableRedactLog L1
pub fn with_enable_redact_log(enable_redact_log: String) -> EvalCtxOption {
    Box::new(move |s| s.enable_redact_log = enable_redact_log)
}

/// WithUserVarsReader set the user variables reader for the `EvalContext`.
// go: WithUserVarsReader L1
pub fn with_user_vars_reader(vars: UserVarsReaderRef) -> EvalCtxOption {
    Box::new(move |s| s.user_vars = Some(vars))
}

/// The default SQL mode of a static context (Go's package-level `defaultSQLMode`).
static defaultSQLMode: LazyLock<SQLMode> = LazyLock::new(|| {
    let (mode, err) = get_sql_mode(DefaultSQLMode);
    if let Err(e) = err {
        panic!("{}", e);
    }
    mode
});

/// The warning sink of an `EvalContext`'s `types.Context` and `errctx.Context` (Go passes the
/// context itself): forwards to the context's warn handler.
#[derive(Default)]
pub struct EvalWarnSink {
    /// Set once, when the options have been applied: Go's `EvalContext` never swaps its
    /// handler after construction.
    handler: OnceLock<Option<WarnHandlerRef>>,
}

impl WarnAppender for EvalWarnSink {
    // glue: EvalContext.AppendWarning through the WarnAppender interface
    fn append_warning(&self, err: Error) {
        if let Some(Some(h)) = self.handler.get() {
            h.append_warning(err);
        }
    }
    // glue: EvalContext.AppendNote through the WarnAppender interface
    fn append_note(&self, err: Error) {
        if let Some(Some(h)) = self.handler.get() {
            h.append_note(err);
        }
    }
}

/// EvalContext should be used as a static context to evaluate an expression.
pub struct EvalContext {
    id: u64,
    state: EvalCtxState,
}

/// NewEvalContext creates a new `EvalContext` with the given options.
// go: NewEvalContext L1
pub fn new_eval_context(opt: Vec<EvalCtxOption>) -> Arc<EvalContext> {
    let sink = Arc::new(EvalWarnSink::default());
    let appender: WarnAppenderRef = Arc::clone(&sink) as WarnAppenderRef;
    let mut state = EvalCtxState {
        warn_handler: None,
        sql_mode: *defaultSQLMode,
        type_ctx: new_context(StrictFlags, utc(), Arc::clone(&appender)),
        err_ctx: errctx::new_context(appender),
        current_db: String::new(),
        current_time: Arc::new(TimeOnce {
            time: Mutex::new(None),
            time_fn: None,
        }),
        max_allowed_packet: DefMaxAllowedPacket,
        enable_redact_log: DefTiDBRedactLog.to_owned(),
        default_week_format_mode: DefDefaultWeekFormat.to_owned(),
        div_precision_increment: DefDivPrecisionIncrement,
        param_list: Vec::new(),
        user_vars: None,
        props: OptionalEvalPropProviders::default(),
    };
    for o in opt {
        o(&mut state);
    }
    if state.warn_handler.is_none() {
        state.warn_handler = Some(Arc::new(new_static_warn_handler(0)));
    }
    if state.user_vars.is_none() {
        state.user_vars = Some(new_user_vars() as UserVarsReaderRef);
    }
    let _ = sink.handler.set(state.warn_handler.clone());
    Arc::new(EvalContext {
        id: gen_context_id(),
        state,
    })
}

impl WarnAppender for EvalContext {
    /// AppendWarning append warnings to the context.
    // go: EvalContext.AppendWarning L1
    fn append_warning(&self, err: Error) {
        if let Some(h) = &self.state.warn_handler {
            h.append_warning(err);
        }
    }

    /// AppendNote appends notes to the context.
    // go: EvalContext.AppendNote L1
    fn append_note(&self, err: Error) {
        if let Some(h) = &self.state.warn_handler {
            h.append_note(err);
        }
    }
}

impl WarnHandler for EvalContext {
    /// WarningCount gets warning count.
    // go: EvalContext.WarningCount L1
    fn warning_count(&self) -> i64 {
        match &self.state.warn_handler {
            Some(h) => h.warning_count(),
            None => 0,
        }
    }

    /// TruncateWarnings truncates warnings begin from start and returns the truncated
    /// warnings.
    // go: EvalContext.TruncateWarnings L1
    fn truncate_warnings(&self, start: i64) -> Vec<SQLWarn> {
        match &self.state.warn_handler {
            Some(h) => h.truncate_warnings(start),
            None => Vec::new(),
        }
    }

    /// CopyWarnings copies warnings to dst slice.
    // go: EvalContext.CopyWarnings L1
    fn copy_warnings(&self, dst: Vec<SQLWarn>) -> Vec<SQLWarn> {
        match &self.state.warn_handler {
            Some(h) => h.copy_warnings(dst),
            None => Vec::new(),
        }
    }
}

impl ParamValues for EvalContext {
    /// GetParamValue returns the value of the parameter by index.
    // go: EvalContext.GetParamValue L1
    fn get_param_value(&self, idx: i64) -> Result<Datum, Error> {
        if idx < 0 || idx >= self.state.param_list.len() as i64 {
            return Err(err_param_index_exceed_param_counts());
        }
        Ok(self.state.param_list[idx as usize].clone())
    }
}

impl EvalContextTrait for EvalContext {
    /// CtxID returns the context id.
    // go: EvalContext.CtxID L1
    fn ctx_id(&self) -> u64 {
        self.id
    }

    /// SQLMode returns the sql mode
    // go: EvalContext.SQLMode L1
    fn sql_mode(&self) -> SQLMode {
        self.state.sql_mode
    }

    /// TypeCtx returns the types.Context
    // go: EvalContext.TypeCtx L1
    fn type_ctx(&self) -> TypeContext {
        self.state.type_ctx.clone()
    }

    /// ErrCtx returns the errctx.Context
    // go: EvalContext.ErrCtx L1
    fn err_ctx(&self) -> errctx::Context {
        self.state.err_ctx.clone()
    }

    /// Location returns the timezone info
    // go: EvalContext.Location L1
    fn location(&self) -> Arc<Location> {
        self.state.type_ctx.location()
    }

    /// CurrentDB return the current database name
    // go: EvalContext.CurrentDB L1
    fn current_db(&self) -> String {
        self.state.current_db.clone()
    }

    /// CurrentTime returns the current time
    // go: EvalContext.CurrentTime L1
    fn current_time(&self) -> Result<Time, Error> {
        self.state.current_time.get_time(&self.location())
    }

    /// GetMaxAllowedPacket returns the value of the 'max_allowed_packet' system variable.
    // go: EvalContext.GetMaxAllowedPacket L1
    fn get_max_allowed_packet(&self) -> u64 {
        self.state.max_allowed_packet
    }

    /// GetTiDBRedactLog returns the value of the 'tidb_redact_log' system variable.
    // go: EvalContext.GetTiDBRedactLog L1
    fn get_tidb_redact_log(&self) -> String {
        self.state.enable_redact_log.clone()
    }

    /// GetDefaultWeekFormatMode returns the value of the 'default_week_format' system
    /// variable.
    // go: EvalContext.GetDefaultWeekFormatMode L1
    fn get_default_week_format_mode(&self) -> String {
        self.state.default_week_format_mode.clone()
    }

    /// GetDivPrecisionIncrement returns the specified value of DivPrecisionIncrement.
    // go: EvalContext.GetDivPrecisionIncrement L1
    fn get_div_precision_increment(&self) -> i64 {
        self.state.div_precision_increment
    }

    /// GetUserVarsReader returns the user variables.
    // go: EvalContext.GetUserVarsReader L1
    fn get_user_vars_reader(&self) -> UserVarsReaderRef {
        Arc::clone(
            self.state
                .user_vars
                .as_ref()
                .expect("set by NewEvalContext"),
        )
    }

    /// GetOptionalPropSet returns the optional properties provided by this context.
    // go: EvalContext.GetOptionalPropSet L1
    fn get_optional_prop_set(&self) -> OptionalEvalPropKeySet {
        self.state.props.prop_key_set()
    }

    /// GetOptionalPropProvider gets the optional property provider by key
    // go: EvalContext.GetOptionalPropProvider L1
    fn get_optional_prop_provider(
        &self,
        key: OptionalEvalPropKey,
    ) -> Option<OptionalEvalPropProviderRef> {
        self.state.props.get(key)
    }
}

impl StaticConvertibleEvalContext for EvalContext {
    /// AllParamValues implements context.StaticConvertibleEvalContext.
    // go: EvalContext.AllParamValues L1
    fn all_param_values(&self) -> Vec<Datum> {
        self.state.param_list.clone()
    }

    /// GetWarnHandler implements context.StaticConvertibleEvalContext.
    // go: EvalContext.GetWarnHandler L1
    fn get_warn_handler(&self) -> WarnHandlerRef {
        Arc::clone(
            self.state
                .warn_handler
                .as_ref()
                .expect("set by NewEvalContext"),
        )
    }
}

impl EvalContext {
    /// Apply returns a new `EvalContext` with the fields updated according to the given
    /// options.
    // go: EvalContext.Apply L1
    pub fn apply(self: &Arc<Self>, opt: Vec<EvalCtxOption>) -> Arc<EvalContext> {
        let sink = Arc::new(EvalWarnSink::default());
        let appender: WarnAppenderRef = Arc::clone(&sink) as WarnAppenderRef;
        let mut state = self.state.clone();
        let old = Arc::clone(self);
        state.current_time = Arc::new(TimeOnce {
            time: Mutex::new(None),
            time_fn: Some(Arc::new(move || old.current_time())),
        });
        state.type_ctx = new_context(
            self.state.type_ctx.flags(),
            self.state.type_ctx.location(),
            Arc::clone(&appender),
        );
        state.err_ctx = errctx::new_context_with_levels(self.state.err_ctx.level_map(), appender);
        for o in opt {
            o(&mut state);
        }
        let _ = sink.handler.set(state.warn_handler.clone());
        Arc::new(EvalContext {
            id: gen_context_id(),
            state,
        })
    }

    /// LoadSystemVars loads system variables and returns a new `EvalContext` with system
    /// variables loaded.
    // go: EvalContext.LoadSystemVars L1 [M-MAPORDER]
    pub fn load_system_vars(
        self: &Arc<Self>,
        sys_vars: &HashMap<String, String>,
    ) -> Result<Arc<EvalContext>, Error> {
        let session_vars = new_session_vars_with_system_variables(sys_vars)?;
        Ok(self.load_session_vars_internal(&session_vars, sys_vars))
    }

    // go: EvalContext.loadSessionVarsInternal L1 [M-MAPORDER]
    pub(crate) fn load_session_vars_internal(
        self: &Arc<Self>,
        session_vars: &SessionVars,
        sys_vars: &HashMap<String, String>,
    ) -> Arc<EvalContext> {
        let mut opts: Vec<EvalCtxOption> = Vec::with_capacity(8);
        for (name, val) in sys_vars {
            let name = tidb_base::gostd::to_lower(name);
            match name.as_str() {
                n if n == TimeZone => opts.push(with_location(Some(session_vars.location()))),
                n if n == SQLModeVar => opts.push(with_sql_mode(SQLMode(session_vars.sql_mode))),
                n if n == Timestamp => opts.push(with_current_time(
                    self.current_time_func_from_string_val(val),
                )),
                n if n == MaxAllowedPacket => {
                    opts.push(with_max_allowed_packet(session_vars.max_allowed_packet))
                }
                n if n == TiDBRedactLog => opts.push(with_enable_redact_log(
                    session_vars.enable_redact_log.clone(),
                )),
                n if n == DefaultWeekFormat => {
                    opts.push(with_default_week_format_mode(val.clone()))
                }
                n if n == DivPrecisionIncrement => opts.push(with_div_precision_increment(
                    session_vars.div_precision_increment,
                )),
                _ => {}
            }
        }
        self.apply(opts)
    }

    // go: EvalContext.currentTimeFuncFromStringVal L1 [M-F2I]
    fn current_time_func_from_string_val(&self, val: &str) -> TimeFn {
        let val = val.to_owned();
        Arc::new(move || {
            if val == DefTimestamp {
                return Ok(now());
            }
            let (ts, err) =
                tidb_types::convert::str_to_float(&StrictContext, val.as_bytes(), false);
            err?;
            // Go math.Modf: the integer part and the fractional part, both with the sign of ts.
            let seconds = ts.trunc();
            let fractional_seconds = if ts.is_infinite() {
                f64::NAN
            } else {
                ts - seconds
            };
            // [M-F2I] int64(seconds), int64(fractionalSeconds*float64(time.Second))
            Ok(unix(
                f64_to_i64(seconds),
                f64_to_i64(fractional_seconds * 1_000_000_000_f64),
            ))
        })
    }
}

/// `NewSessionVars(nil)`: a session without a global variable accessor. Go's nil interface
/// panics when a method is called; so does this one.
struct NilGlobalVarAccessor;

impl GlobalVarAccessor for NilGlobalVarAccessor {
    // glue: method call on a nil GlobalVarAccessor interface (Go panics)
    fn get_global_sys_var(&self, _name: &str) -> Result<String, Error> {
        panic!("runtime error: invalid memory address or nil pointer dereference")
    }
    // glue: method call on a nil GlobalVarAccessor interface (Go panics)
    fn set_global_sys_var_only(
        &mut self,
        _name: &str,
        _value: &str,
        _update_local: bool,
    ) -> Result<(), Error> {
        panic!("runtime error: invalid memory address or nil pointer dereference")
    }
    // glue: method call on a nil GlobalVarAccessor interface (Go panics)
    fn get_tidb_table_value(&self, _name: &str) -> Result<String, Error> {
        panic!("runtime error: invalid memory address or nil pointer dereference")
    }
    // glue: method call on a nil GlobalVarAccessor interface (Go panics)
    fn set_tidb_table_value(
        &mut self,
        _name: &str,
        _value: &str,
        _comment: &str,
    ) -> Result<(), Error> {
        panic!("runtime error: invalid memory address or nil pointer dereference")
    }
}

// go: newSessionVarsWithSystemVariables L1 [M-MAPORDER]
pub(crate) fn new_session_vars_with_system_variables(
    vars: &HashMap<String, String>,
) -> Result<SessionVars, Error> {
    let mut session_vars = SessionVars::new(Box::new(NilGlobalVarAccessor));
    type NameVal<'a> = Option<(&'a String, &'a String)>;
    let (mut cs, mut col): (NameVal<'_>, NameVal<'_>) = (None, None);
    for (name, val) in vars {
        let lower = tidb_base::gostd::to_lower(name);
        if lower == CharacterSetConnection {
            cs = Some((name, val));
        } else if lower == CollationConnection {
            col = Some((name, val));
        } else {
            session_vars.set_system_var(name, val)?;
        }
    }
    if let Some((n, v)) = cs {
        session_vars.set_system_var(n, v)?;
    }
    if let Some((n, v)) = col {
        session_vars.set_system_var(n, v)?;
    }
    Ok(session_vars)
}

/// MakeEvalContextStatic converts the `exprctx.StaticConvertibleEvalContext` to `EvalContext`.
// go: MakeEvalContextStatic L1
pub fn make_eval_context_static(ctx: &StaticConvertibleEvalContextRef) -> Arc<EvalContext> {
    let type_ctx = ctx.type_ctx();
    let err_ctx = ctx.err_ctx();

    // TODO: at least provide some optional eval prop provider which is suitable to be used
    // in the static context.
    let props: Vec<OptionalEvalPropProviderRef> = Vec::new();

    let params = ctx.all_param_values();

    // TODO: use a more structural way to replace the closure.
    // These closure makes sure the fields which may be changed in the execution of the next
    // statement will not be embedded into them, to make sure it's safe to call them after
    // the session continues to execute other statements.
    let current_time = ctx.current_time();
    new_eval_context(vec![
        with_warn_handler(Some(ctx.get_warn_handler())),
        with_sql_mode(ctx.sql_mode()),
        with_type_flags(type_ctx.flags()),
        with_location(Some(type_ctx.location())),
        with_err_level_map(err_ctx.level_map()),
        with_current_db(ctx.current_db()),
        with_current_time(Arc::new(move || current_time.clone())),
        with_max_allowed_packet(ctx.get_max_allowed_packet()),
        with_default_week_format_mode(ctx.get_default_week_format_mode()),
        with_div_precision_increment(ctx.get_div_precision_increment()),
        with_param_list(&params),
        with_user_vars_reader(ctx.get_user_vars_reader().clone_reader()),
        with_optional_property(props),
        with_enable_redact_log(ctx.get_tidb_redact_log()),
    ])
}
