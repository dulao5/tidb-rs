//! go-file: pkg/util/context/warn.go
//! go-file: pkg/util/context/plancache.go
//! go-file: pkg/util/context/context.go
//!
//! `pkg/util/context` (block D00.1), in tidb-base: it only needs [`Error`], and the types
//! below it (`types.Context` of D01.1, `errctx` of D00.1) take its `WarnAppender`.
//!
//! Mapping:
//! - Go `error` values are [`Error`] (M-ERR); a nil error is not representable, so the
//!   functions that take an `error` take an [`Error`].
//! - The handlers are shared by reference in Go (interface values holding pointers):
//!   here they are `Arc<dyn WarnAppender + Send + Sync>`, and the methods take `&self` with
//!   the state behind a mutex (Go's `sync.Mutex`).
//! - Slices returned "not copied" by Go (`GetWarnings`) are clones (L2): Go callers must not
//!   modify them, so the copy is not observable.

// Constants and statics keep their Go names (mapping rule M-GONAME).
#![allow(non_upper_case_globals)]

use std::any::Any;
use std::borrow::Cow;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Once};

use crate::Arg;
use crate::errors::Error;
use crate::gofmt::sprintf;
use crate::gostd::json::DecodeError;
use crate::gostd::json::{self, Decoder, Raw};

/// WarnLevelError represents level "Error" for 'SHOW WARNINGS' syntax.
pub const WarnLevelError: &str = "Error";
/// WarnLevelWarning represents level "Warning" for 'SHOW WARNINGS' syntax.
pub const WarnLevelWarning: &str = "Warning";
/// WarnLevelNote represents level "Note" for 'SHOW WARNINGS' syntax.
pub const WarnLevelNote: &str = "Note";

/// SQLWarn relates a sql warning and it's level.
#[derive(Clone, Debug, PartialEq)]
pub struct SQLWarn {
    pub level: Cow<'static, str>,
    pub err: Error,
}

/// The field names of `jsonSQLWarn` (`level`, `err`, `msg`).
const JSON_SQL_WARN_FIELDS: &[&str] = &["level", "err", "msg"];

impl SQLWarn {
    /// MarshalJSON implements the Marshaler.MarshalJSON interface.
    ///
    /// `json.Marshal(&jsonSQLWarn{...})`: `err` (a `*terror.Error`) when the cause is one,
    /// else `msg` (`e.Error()`); both are `omitempty`.
    // go: SQLWarn.MarshalJSON L1 [M-ERR]
    pub fn marshal_json(&self) -> Result<Vec<u8>, Error> {
        let mut out = String::from("{\"level\":");
        json::append_string(&mut out, &self.level);
        let e = crate::errors::cause(&self.err);
        if e.is_normalized() {
            // Omit outter errors because only the most inner error matters.
            out.push_str(",\"err\":");
            out.push_str(&e.marshal_json());
        } else {
            let msg = e.error();
            if !msg.is_empty() {
                out.push_str(",\"msg\":");
                json::append_go_string(&mut out, &msg);
            }
        }
        out.push('}');
        Ok(out.into_bytes())
    }

    /// UnmarshalJSON implements the Unmarshaler.UnmarshalJSON interface.
    // go: SQLWarn.UnmarshalJSON L1 [M-ERR]
    pub fn unmarshal_json(&mut self, data: &[u8]) -> Result<(), Error> {
        let raw = json::unmarshal_raw(data).map_err(Error::plain)?;
        let (mut level, mut msg) = (String::new(), String::new());
        let mut sql_err: Option<Error> = None;
        let mut d = Decoder::new();
        d.object(
            &raw,
            "jsonSQLWarn",
            "context.jsonSQLWarn",
            JSON_SQL_WARN_FIELDS,
            |d, i, v| match i {
                0 => d.string(v, &mut level),
                1 => match v {
                    Raw::Null => sql_err = None,
                    v => {
                        let mut e = sql_err.take().unwrap_or_default();
                        match e.unmarshal_json_raw(v) {
                            Ok(()) => sql_err = Some(e),
                            Err(err) => {
                                sql_err = Some(e);
                                d.fail(err);
                            }
                        }
                    }
                },
                2 => d.string(v, &mut msg),
                _ => {}
            },
        );
        d.finish()
            .map_err(|e: DecodeError| Error::plain(e.to_string()))?;
        self.level = Cow::Owned(level);
        self.err = match sql_err {
            Some(e) => e,
            None => crate::errors::new(msg),
        };
        Ok(())
    }
}

/// WarnAppender provides a function to add a warning. Using interface rather than a simple
/// function/closure can avoid memory allocation in some cases. See
/// https://github.com/pingcap/tidb/issues/49277
pub trait WarnAppender {
    /// AppendWarning appends a warning
    // glue: interface method of contextutil.WarnAppender
    fn append_warning(&self, err: Error);
    /// AppendNote appends a warning with level 'Note'.
    // glue: interface method of contextutil.WarnAppender
    fn append_note(&self, msg: Error);
}

/// A shared `WarnAppender` interface value.
pub type WarnAppenderRef = Arc<dyn WarnAppender + Send + Sync>;

/// WarnHandler provides a handler to append and get warnings.
pub trait WarnHandler: WarnAppender {
    /// WarningCount gets warning count.
    // glue: interface method of contextutil.WarnHandler
    fn warning_count(&self) -> i64;
    /// TruncateWarnings truncates warnings begin from start and returns the truncated warnings.
    ///
    /// Deprecated in Go: it's unsafe to read the warnings returned by `GetWarnings` after
    /// truncate and append the warnings.
    // glue: interface method of contextutil.WarnHandler
    fn truncate_warnings(&self, start: i64) -> Vec<SQLWarn>;
    /// CopyWarnings copies warnings to another slice. The input argument provides target
    /// warnings that copy to.
    // glue: interface method of contextutil.WarnHandler
    fn copy_warnings(&self, dst: Vec<SQLWarn>) -> Vec<SQLWarn>;
}

/// WarnHandlerExt includes more methods for WarnHandler. It allows more detailed control
/// over warnings.
pub trait WarnHandlerExt: WarnHandler {
    /// AppendWarnings appends multiple warnings
    // glue: interface method of contextutil.WarnHandlerExt
    fn append_warnings(&self, warns: &[SQLWarn]);
    /// AppendError appends a warning with level 'Error'.
    // glue: interface method of contextutil.WarnHandlerExt
    fn append_error(&self, warn: Error);
    /// GetWarnings gets all warnings.
    // glue: interface method of contextutil.WarnHandlerExt
    fn get_warnings(&self) -> Vec<SQLWarn>;
    /// SetWarnings resets all warnings in the handler directly. The handler may ignore the
    /// given warnings.
    // glue: interface method of contextutil.WarnHandlerExt
    fn set_warnings(&self, warns: Vec<SQLWarn>);
    /// NumErrorWarnings returns the number of warnings with level 'Error' and the total
    /// number of warnings.
    // glue: interface method of contextutil.WarnHandlerExt
    fn num_error_warnings(&self) -> (u16, i64);
    /// The type assertion `h.(interface{ Reset() })` followed by `h.Reset()`: resets the
    /// handler and returns true if it has a `Reset` method.
    // glue: Go type assertion interface{ Reset() } (StatementContext.Reset)
    fn try_reset(&self) -> bool {
        false
    }
}

/// A shared `WarnHandlerExt` interface value.
pub type WarnHandlerExtRef = Arc<dyn WarnHandlerExt + Send + Sync>;

/// StaticWarnHandler implements the WarnHandler interface.
#[derive(Debug, Default)]
pub struct StaticWarnHandler {
    warnings: Mutex<Vec<SQLWarn>>,
}

/// NewStaticWarnHandler creates a new StaticWarnHandler.
///
/// Go keeps a nil slice when `sliceCap <= 0`; nil and empty are not told apart by any
/// method of the handler.
// go: NewStaticWarnHandler L1
pub fn new_static_warn_handler(slice_cap: i64) -> StaticWarnHandler {
    let warnings = if slice_cap > 0 {
        Vec::with_capacity(slice_cap as usize)
    } else {
        Vec::new()
    };
    StaticWarnHandler {
        warnings: Mutex::new(warnings),
    }
}

/// NewStaticWarnHandlerWithHandler creates a new StaticWarnHandler with copying the warnings
/// from the given WarnHandler.
// go: NewStaticWarnHandlerWithHandler L1
pub fn new_static_warn_handler_with_handler(h: Option<&dyn WarnHandler>) -> StaticWarnHandler {
    let Some(h) = h else {
        return new_static_warn_handler(0);
    };
    let cnt = h.warning_count();
    let new_handler = new_static_warn_handler(cnt);
    if cnt > 0 {
        let mut w = new_handler.lock();
        let dst = std::mem::take(&mut *w);
        *w = h.copy_warnings(dst);
    }
    new_handler
}

impl StaticWarnHandler {
    // glue: sync.Mutex.Lock (a panic while held does not poison the warnings, as in Go)
    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<SQLWarn>> {
        self.warnings.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Reset resets the warnings of this handler.
    // go: StaticWarnHandler.Reset L1
    pub fn reset(&self) {
        self.lock().clear();
    }

    // go: StaticWarnHandler.appendWarningWithLevel L1
    fn append_warning_with_level(w: &mut Vec<SQLWarn>, level: &'static str, warn: Error) {
        if w.len() < u16::MAX as usize {
            w.push(SQLWarn {
                level: Cow::Borrowed(level),
                err: warn,
            });
        }
    }
}

impl WarnAppender for StaticWarnHandler {
    /// AppendWarning implements the StaticWarnHandler.AppendWarning.
    // go: StaticWarnHandler.AppendWarning L1
    fn append_warning(&self, warn: Error) {
        StaticWarnHandler::append_warning_with_level(&mut self.lock(), WarnLevelWarning, warn);
    }

    /// AppendNote appends a warning with level 'Note'.
    // go: StaticWarnHandler.AppendNote L1
    fn append_note(&self, warn: Error) {
        StaticWarnHandler::append_warning_with_level(&mut self.lock(), WarnLevelNote, warn);
    }
}

impl WarnHandler for StaticWarnHandler {
    /// WarningCount implements the StaticWarnHandler.WarningCount.
    // go: StaticWarnHandler.WarningCount L1
    fn warning_count(&self) -> i64 {
        self.lock().len() as i64
    }

    /// TruncateWarnings implements the StaticWarnHandler.TruncateWarnings.
    ///
    /// A negative `start` larger than nothing panics as Go's slice expression does.
    // go: StaticWarnHandler.TruncateWarnings L1 [M-PANIC]
    fn truncate_warnings(&self, start: i64) -> Vec<SQLWarn> {
        let mut w = self.lock();
        let sz = w.len() as i64 - start;
        if sz <= 0 {
            return Vec::new();
        }
        if start < 0 {
            panic!("runtime error: slice bounds out of range [{start}:]");
        }
        w.split_off(start as usize)
    }

    /// CopyWarnings implements the StaticWarnHandler.CopyWarnings.
    // go: StaticWarnHandler.CopyWarnings L1
    fn copy_warnings(&self, mut dst: Vec<SQLWarn>) -> Vec<SQLWarn> {
        let w = self.lock();
        dst.clear();
        dst.extend(w.iter().cloned());
        dst
    }
}

impl WarnHandlerExt for StaticWarnHandler {
    /// AppendWarnings appends multiple warnings
    // go: StaticWarnHandler.AppendWarnings L1
    fn append_warnings(&self, warns: &[SQLWarn]) {
        let mut w = self.lock();
        if w.len() < u16::MAX as usize {
            w.extend(warns.iter().cloned());
        }
    }

    /// AppendError appends a warning with level 'Error'.
    // go: StaticWarnHandler.AppendError L1
    fn append_error(&self, warn: Error) {
        StaticWarnHandler::append_warning_with_level(&mut self.lock(), WarnLevelError, warn);
    }

    /// GetWarnings returns all warnings in the handler. It's not safe to modify the returned
    /// slice.
    // go: StaticWarnHandler.GetWarnings L2 (returns a copy; Go callers do not modify the slice)
    fn get_warnings(&self) -> Vec<SQLWarn> {
        self.lock().clone()
    }

    /// SetWarnings sets the internal warnings directly.
    // go: StaticWarnHandler.SetWarnings L1
    fn set_warnings(&self, warns: Vec<SQLWarn>) {
        *self.lock() = warns;
    }

    // glue: StaticWarnHandler has a Reset method
    fn try_reset(&self) -> bool {
        self.reset();
        true
    }

    /// NumErrorWarnings returns the number of warnings with level 'Error' and the total
    /// number of warnings.
    // go: StaticWarnHandler.NumErrorWarnings L1
    fn num_error_warnings(&self) -> (u16, i64) {
        let w = self.lock();
        let mut num_error: u16 = 0;
        for x in w.iter() {
            if x.level == WarnLevelError {
                num_error = num_error.wrapping_add(1);
            }
        }
        (num_error, w.len() as i64)
    }
}

/// `ignoreWarn`: the handler behind [`IgnoreWarn`].
#[derive(Debug, Default)]
pub struct IgnoreWarnHandler;

impl WarnAppender for IgnoreWarnHandler {
    // go: ignoreWarn.AppendWarning L1
    fn append_warning(&self, _err: Error) {}

    // go: ignoreWarn.AppendNote L1
    fn append_note(&self, _err: Error) {}
}

impl WarnHandler for IgnoreWarnHandler {
    // go: ignoreWarn.WarningCount L1
    fn warning_count(&self) -> i64 {
        0
    }

    // go: ignoreWarn.TruncateWarnings L1
    fn truncate_warnings(&self, _start: i64) -> Vec<SQLWarn> {
        Vec::new()
    }

    // go: ignoreWarn.CopyWarnings L1
    fn copy_warnings(&self, _dst: Vec<SQLWarn>) -> Vec<SQLWarn> {
        Vec::new()
    }
}

/// IgnoreWarn is WarnHandler which does nothing
pub static IgnoreWarn: IgnoreWarnHandler = IgnoreWarnHandler;

/// [`IgnoreWarn`] as a shared `WarnAppender` interface value.
// glue: the interface value contextutil.IgnoreWarn as an Arc
pub fn ignore_warn_appender() -> WarnAppenderRef {
    Arc::new(IgnoreWarnHandler)
}

/// `funcWarnAppender`: the WarnAppender of [`new_func_warn_appender_for_test`].
pub struct FuncWarnAppender {
    func: WarnFunc,
}

/// The function of a [`FuncWarnAppender`]: `fn(level string, err error)`.
pub type WarnFunc = Box<dyn Fn(&str, Error) + Send + Sync>;

impl WarnAppender for FuncWarnAppender {
    // go: funcWarnAppender.AppendWarning L1
    fn append_warning(&self, err: Error) {
        (self.func)(WarnLevelWarning, err);
    }

    // go: funcWarnAppender.AppendNote L1
    fn append_note(&self, err: Error) {
        (self.func)(WarnLevelNote, err);
    }
}

/// NewFuncWarnAppenderForTest creates a `WarnHandler` which will use the function to handle
/// warn. To have a better performance, it's not suggested to use this function in
/// production.
// go: NewFuncWarnAppenderForTest L1
pub fn new_func_warn_appender_for_test(
    func: impl Fn(&str, Error) + Send + Sync + 'static,
) -> FuncWarnAppender {
    FuncWarnAppender {
        func: Box::new(func),
    }
}

// ---------------------------------------------------------------- plancache.go

/// PlanCacheType is the flag of plan cache
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PlanCacheType(pub i64);

/// DefaultNoCache no cache
pub const DefaultNoCache: PlanCacheType = PlanCacheType(0);
/// SessionPrepared session prepared plan cache
pub const SessionPrepared: PlanCacheType = PlanCacheType(1);
/// SessionNonPrepared session non-prepared plan cache
pub const SessionNonPrepared: PlanCacheType = PlanCacheType(2);

/// The mutable state of a [`PlanCacheTracker`] (Go: its fields under `mu`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PlanCacheState {
    pub use_cache: bool,
    pub cache_type: PlanCacheType,
    pub plan_cache_unqualified: String,
    /// force the optimizer to use plan cache even if there is risky optimization, see #49736.
    pub force_plan_cache: bool,
    pub always_warn_skip_cache: bool,
}

/// PlanCacheTracker `PlanCacheTracker`. `PlanCacheTracker` is thread-safe.
pub struct PlanCacheTracker {
    mu: Mutex<PlanCacheState>,
    warn_handler: Option<WarnAppenderRef>,
}

impl std::fmt::Debug for PlanCacheTracker {
    // glue: Debug for a struct holding a trait object
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlanCacheTracker")
            .field("mu", &*self.lock())
            .finish()
    }
}

/// `errors.NewNoStackErrorf`: a plain error without a stack.
// go-dep: github.com/pingcap/errors@v0.11.5-0.20250523034308-74f78ae071ee.NewNoStackErrorf L1 [M-ERR]
fn new_no_stack_errorf(format: &str, args: &[Arg]) -> Error {
    Error::plain(sprintf(format, args))
}

impl PlanCacheTracker {
    // glue: sync.Mutex.Lock
    fn lock(&self) -> std::sync::MutexGuard<'_, PlanCacheState> {
        self.mu.lock().unwrap_or_else(|e| e.into_inner())
    }

    // glue: h.warnHandler.AppendWarning on a possibly nil interface (Go would panic)
    fn append_warning(&self, err: Error) {
        match &self.warn_handler {
            Some(w) => w.append_warning(err),
            None => panic!("runtime error: invalid memory address or nil pointer dereference"),
        }
    }

    /// WarnSkipPlanCache output the reason why this query can't hit the plan cache.
    // go: PlanCacheTracker.WarnSkipPlanCache L1
    pub fn warn_skip_plan_cache(&self, reason: &str) {
        let mut s = self.lock();
        if s.cache_type == DefaultNoCache {
            return;
        }
        self.warn_skip_plan_cache_locked(&mut s, reason);
    }

    /// SetSkipPlanCache sets to skip plan cache.
    // go: PlanCacheTracker.SetSkipPlanCache L1
    pub fn set_skip_plan_cache(&self, reason: &str) {
        let mut s = self.lock();
        if !s.use_cache {
            return;
        }
        if s.force_plan_cache {
            self.append_warning(new_no_stack_errorf(
                "force plan-cache: may use risky cached plan: %s",
                &[Arg::from(reason.to_owned())],
            ));
            return;
        }
        s.use_cache = false;
        self.warn_skip_plan_cache_locked(&mut s, reason);
    }

    // go: PlanCacheTracker.warnSkipPlanCache L1
    fn warn_skip_plan_cache_locked(&self, s: &mut PlanCacheState, reason: &str) {
        reason.clone_into(&mut s.plan_cache_unqualified);
        match s.cache_type {
            DefaultNoCache => self.append_warning(Error::plain("unknown cache type")),
            SessionPrepared => self.append_warning(new_no_stack_errorf(
                "skip prepared plan-cache: %s",
                &[Arg::from(reason.to_owned())],
            )),
            SessionNonPrepared if s.always_warn_skip_cache => {
                // use "plan_cache" rather than types.ExplainFormatPlanCache to avoid import cycle
                self.append_warning(new_no_stack_errorf(
                    "skip non-prepared plan-cache: %s",
                    &[Arg::from(reason.to_owned())],
                ));
            }
            _ => {}
        }
    }

    /// SetAlwaysWarnSkipCache sets whether to always warn when skip plan cache. By default,
    /// for `SessionNonPrepared`, we don't warn when skip plan cache. But in some cases, we
    /// want to warn even for `SessionNonPrepared`.
    // go: PlanCacheTracker.SetAlwaysWarnSkipCache L1
    pub fn set_always_warn_skip_cache(&self, always_warn_skip_cache: bool) {
        self.lock().always_warn_skip_cache = always_warn_skip_cache;
    }

    /// SetCacheType sets the cache type.
    // go: PlanCacheTracker.SetCacheType L1
    pub fn set_cache_type(&self, cache_type: PlanCacheType) {
        self.lock().cache_type = cache_type;
    }

    /// SetForcePlanCache sets whether to force the optimizer to use plan cache even if there
    /// is risky optimization.
    // go: PlanCacheTracker.SetForcePlanCache L1
    pub fn set_force_plan_cache(&self, force_plan_cache: bool) {
        self.lock().force_plan_cache = force_plan_cache;
    }

    /// EnablePlanCache sets to use plan cache.
    // go: PlanCacheTracker.EnablePlanCache L1
    pub fn enable_plan_cache(&self) {
        self.lock().use_cache = true;
    }

    /// Save captures the mutable planning-time state of the tracker.
    // go: PlanCacheTracker.Save L1
    pub fn save(&self) -> PlanCacheState {
        self.lock().clone()
    }

    /// Restore restores the mutable planning-time state of the tracker.
    // go: PlanCacheTracker.Restore L1
    pub fn restore(&self, state: PlanCacheState) {
        *self.lock() = state;
    }

    /// UseCache returns whether to use plan cache.
    // go: PlanCacheTracker.UseCache L1
    pub fn use_cache(&self) -> bool {
        self.lock().use_cache
    }

    /// PlanCacheUnqualified returns the reason of why the plan cache is unqualified
    // go: PlanCacheTracker.PlanCacheUnqualified L1
    pub fn plan_cache_unqualified(&self) -> String {
        self.lock().plan_cache_unqualified.clone()
    }
}

/// NewPlanCacheTracker creates a new PlanCacheTracker.
// go: NewPlanCacheTracker L1
pub fn new_plan_cache_tracker(warn_handler: Option<WarnAppenderRef>) -> PlanCacheTracker {
    PlanCacheTracker {
        mu: Mutex::new(PlanCacheState::default()),
        warn_handler,
    }
}

/// RangeFallbackHandler is used to handle range fallback. If there are too many ranges,
/// it'll fallback and add a warning. RangeFallbackHandler is thread-safe.
pub struct RangeFallbackHandler {
    plan_cache_tracker: Arc<PlanCacheTracker>,
    warn_handler: WarnAppenderRef,
    report_range_fallback_warning: Once,
}

impl RangeFallbackHandler {
    /// RecordRangeFallback records the range fallback event.
    // go: RangeFallbackHandler.RecordRangeFallback L1
    pub fn record_range_fallback(&self, range_max_size: i64) {
        // If range fallback happens, it means ether the query is unreasonable(for example,
        // several long IN lists) or tidb_opt_range_max_size is too small and the generated
        // plan is probably suboptimal. In that case we don't put it into plan cache.
        self.plan_cache_tracker
            .set_skip_plan_cache("in-list is too long");
        self.report_range_fallback_warning.call_once(|| {
            self.warn_handler.append_warning(new_no_stack_errorf(
                "Memory capacity of %v bytes for 'tidb_opt_range_max_size' exceeded when building ranges. Less accurate ranges such as full range are chosen",
                &[Arg::from(range_max_size)],
            ));
        });
    }
}

/// NewRangeFallbackHandler creates a new RangeFallbackHandler.
// go: NewRangeFallbackHandler L1
pub fn new_range_fallback_handler(
    plan_cache_tracker: Arc<PlanCacheTracker>,
    warn_handler: WarnAppenderRef,
) -> RangeFallbackHandler {
    RangeFallbackHandler {
        plan_cache_tracker,
        warn_handler,
        report_range_fallback_warning: Once::new(),
    }
}

// ---------------------------------------------------------------- context.go

/// ValueStoreContext is a context that can store values.
///
/// Go keys are `fmt.Stringer` values compared by identity; here a key is a `&'static str`
/// naming it (the Go key types are unexported singletons).
pub trait ValueStoreContext {
    /// SetValue saves a value associated with this context for key.
    // glue: interface method of contextutil.ValueStoreContext
    fn set_value(&self, key: &'static str, value: Arc<dyn Any + Send + Sync>);
    /// Value returns the value associated with this context for key.
    // glue: interface method of contextutil.ValueStoreContext
    fn value(&self, key: &'static str) -> Option<Arc<dyn Any + Send + Sync>>;
    /// ClearValue clears the value associated with this context for key.
    // glue: interface method of contextutil.ValueStoreContext
    fn clear_value(&self, key: &'static str);
    /// GetDomain returns the domain.
    // glue: interface method of contextutil.ValueStoreContext
    fn get_domain(&self) -> Option<Arc<dyn Any + Send + Sync>>;
}

static CONTEXT_ID_GENERATOR: AtomicU64 = AtomicU64::new(0);

/// GenContextID generates a unique context ID.
// go: GenContextID L1
pub fn gen_context_id() -> u64 {
    CONTEXT_ID_GENERATOR.fetch_add(1, Ordering::SeqCst) + 1
}
