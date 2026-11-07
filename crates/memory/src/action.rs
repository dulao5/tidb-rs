//! go-file: pkg/util/memory/action.go
//!
//! The actions a [`Tracker`] runs when it exceeds its quota (A1 3.1: the chain, priorities and
//! `reArrangeFallback` are user-visible and L1). Go's `panic(err)` in `PanicOnExceed.Action`
//! becomes an `Err` (N3 3.4: "panic → Result", L2).

// Priority constants keep their Go names (mapping rule M-GONAME).
#![allow(non_upper_case_globals)]

use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::atomic::AtomicI32;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering::SeqCst;

use parking_lot::Mutex;
use tidb_base::Error;
use tidb_base::dbterror::ClassUtil;
use tidb_base::errno;
use tidb_base::go_args;

use crate::sqlkiller;
use crate::sqlkiller::SqlKiller;
use crate::tracker::Tracker;

/// A shared action (Go: an `ActionOnExceed` interface value, compared by identity).
pub type ActionRef = Arc<dyn ActionOnExceed>;

/// Go `ActionOnExceed`: the action when memory usage exceeds memory quota.
pub trait ActionOnExceed: Send + Sync {
    // glue: Go interface method ActionOnExceed.Action
    /// Called when memory usage exceeds memory quota by the corresponding Tracker.
    fn action(&self, t: &Arc<Tracker>) -> Result<(), Error>;
    // glue: Go interface method ActionOnExceed.SetFallback
    /// Sets a fallback action which will be triggered if itself has already been triggered.
    fn set_fallback(&self, a: Option<ActionRef>);
    // glue: Go interface method ActionOnExceed.GetFallback
    /// The fallback action of the action.
    fn get_fallback(&self) -> Option<ActionRef>;
    // glue: Go interface method ActionOnExceed.GetPriority
    /// The priority of the action.
    fn get_priority(&self) -> i64;
    // glue: Go interface method ActionOnExceed.SetFinished
    /// Sets the finished state of the action.
    fn set_finished(&self);
    // glue: Go interface method ActionOnExceed.IsFinished
    /// The finished state of the action.
    fn is_finished(&self) -> bool;
}

/// Identity of an action (Go `==` on interface values of pointer type).
// glue: Go interface equality is pointer identity
pub(crate) fn same_action(a: &ActionRef, b: &ActionRef) -> bool {
    std::ptr::addr_eq(Arc::as_ptr(a), Arc::as_ptr(b))
}

/// Go `actionWithPriority`: wraps an action and overrides its priority.
pub struct ActionWithPriority {
    inner: ActionRef,
    priority: i64,
}

// go: NewActionWithPriority L1
pub fn new_action_with_priority(action: ActionRef, priority: i64) -> Arc<ActionWithPriority> {
    Arc::new(ActionWithPriority {
        inner: action,
        priority,
    })
}

impl ActionOnExceed for ActionWithPriority {
    // glue: Go embeds the wrapped interface; the call is forwarded
    fn action(&self, t: &Arc<Tracker>) -> Result<(), Error> {
        self.inner.action(t)
    }
    // glue: forwarded to the embedded action
    fn set_fallback(&self, a: Option<ActionRef>) {
        self.inner.set_fallback(a)
    }
    // glue: forwarded to the embedded action
    fn get_fallback(&self) -> Option<ActionRef> {
        self.inner.get_fallback()
    }
    // go: actionWithPriority.GetPriority L1
    fn get_priority(&self) -> i64 {
        self.priority
    }
    // glue: forwarded to the embedded action
    fn set_finished(&self) {
        self.inner.set_finished()
    }
    // glue: forwarded to the embedded action
    fn is_finished(&self) -> bool {
        self.inner.is_finished()
    }
}

/// Go `BaseOOMAction`: the fallback and finished state shared by all actions. Go reads and
/// writes `fallbackAction` without a lock; here it is behind a `Mutex` (no other change).
#[derive(Default)]
pub struct BaseOOMAction {
    fallback_action: Mutex<Option<ActionRef>>,
    finished: AtomicI32,
}

impl BaseOOMAction {
    // go: BaseOOMAction.SetFallback L1
    pub fn set_fallback(&self, a: Option<ActionRef>) {
        *self.fallback_action.lock() = a;
    }

    // go: BaseOOMAction.SetFinished L1
    pub fn set_finished(&self) {
        self.finished.store(1, SeqCst);
    }

    // go: BaseOOMAction.IsFinished L1
    pub fn is_finished(&self) -> bool {
        self.finished.load(SeqCst) == 1
    }

    /// Go `GetFallback`: skips (and drops) finished fallbacks.
    // go: BaseOOMAction.GetFallback L1
    pub fn get_fallback(&self) -> Option<ActionRef> {
        loop {
            let fb = self.fallback_action.lock().clone();
            match fb {
                Some(f) if f.is_finished() => self.set_fallback(f.get_fallback()),
                fb => return fb,
            }
        }
    }

    // go: BaseOOMAction.TriggerFallBackAction L1
    pub fn trigger_fall_back_action(&self, tracker: &Arc<Tracker>) -> Result<(), Error> {
        if let Some(fallback) = self.get_fallback() {
            fallback.action(tracker)?;
        }
        Ok(())
    }
}

/// Default OOM action priorities (Go `iota`).
pub const DefPanicPriority: i64 = 0;
/// LOG.
pub const DefLogPriority: i64 = 1;
/// Spill to disk.
pub const DefSpillPriority: i64 = 2;
/// Higher than normal disk spill: it can release much more memory in the future, and its
/// performance impact is smaller (write-only in the execution stage).
pub const DefCursorFetchSpillPriority: i64 = 3;
/// Coprocessor rate limit.
pub const DefRateLimitPriority: i64 = 4;

/// Go `func(uint64)` log hook of the LOG / CANCEL actions (called with the connection ID).
pub type LogHook = Arc<dyn Fn(u64) + Send + Sync>;

/// Go `errMemExceedThreshold` (8001).
static errMemExceedThreshold: LazyLock<Error> =
    LazyLock::new(|| ClassUtil.new_std(errno::ErrMemExceedThreshold));

// go-part: LogOnExceed.Action (the 8001 text, shared with PanicOnExceed.Action)
fn mem_exceed_error(t: &Arc<Tracker>) -> Error {
    errMemExceedThreshold.gen_with_stack_by_args(go_args![
        t.label(),
        t.bytes_consumed(),
        t.get_bytes_limit(),
        t.string()
    ])
}

/// Go `LogOnExceed`: logs a warning only once when memory usage exceeds memory quota.
#[derive(Default)]
pub struct LogOnExceed {
    log_hook: Mutex<Option<LogHook>>,
    base: BaseOOMAction,
    /// Go `ConnID`.
    pub conn_id: AtomicU64,
    /// Go `mutex` + `acted`: the mutex guards `acted`.
    acted: Mutex<bool>,
}

impl LogOnExceed {
    // glue: constructor (Go: `&memory.LogOnExceed{ConnID: id}`)
    pub fn new(conn_id: u64) -> Arc<LogOnExceed> {
        let a = LogOnExceed::default();
        a.conn_id.store(conn_id, SeqCst);
        Arc::new(a)
    }

    // go: LogOnExceed.SetLogHook L1
    pub fn set_log_hook(&self, hook: Option<LogHook>) {
        *self.log_hook.lock() = hook;
    }
}

impl ActionOnExceed for LogOnExceed {
    // go: LogOnExceed.Action L1
    fn action(&self, t: &Arc<Tracker>) -> Result<(), Error> {
        let mut acted = self.acted.lock();
        if !*acted {
            *acted = true;
            let hook = self.log_hook.lock().clone();
            match hook {
                None => {
                    let err = mem_exceed_error(t);
                    tracing::warn!(error = %err, "memory exceeds quota");
                }
                Some(h) => h(self.conn_id.load(SeqCst)),
            }
        }
        Ok(())
    }
    // glue: Go embeds BaseOOMAction; forwarded to its SetFallback
    fn set_fallback(&self, a: Option<ActionRef>) {
        self.base.set_fallback(a)
    }
    // glue: Go embeds BaseOOMAction; forwarded to its GetFallback
    fn get_fallback(&self) -> Option<ActionRef> {
        self.base.get_fallback()
    }
    // go: LogOnExceed.GetPriority L1
    fn get_priority(&self) -> i64 {
        DefLogPriority
    }
    // glue: Go embeds BaseOOMAction; forwarded to its SetFinished
    fn set_finished(&self) {
        self.base.set_finished()
    }
    // glue: Go embeds BaseOOMAction; forwarded to its IsFinished
    fn is_finished(&self) -> bool {
        self.base.is_finished()
    }
}

/// Go `PanicOnExceed`: kills the query (8175) when memory usage exceeds memory quota.
#[derive(Default)]
pub struct PanicOnExceed {
    /// Go `Killer`.
    pub killer: Option<Arc<SqlKiller>>,
    log_hook: Mutex<Option<LogHook>>,
    base: BaseOOMAction,
    /// Go `ConnID`.
    pub conn_id: AtomicU64,
    acted: Mutex<bool>,
}

impl PanicOnExceed {
    // glue: constructor (Go: `&memory.PanicOnExceed{Killer: k, ConnID: id}`)
    pub fn new(killer: Option<Arc<SqlKiller>>, conn_id: u64) -> Arc<PanicOnExceed> {
        let a = PanicOnExceed {
            killer,
            ..PanicOnExceed::default()
        };
        a.conn_id.store(conn_id, SeqCst);
        Arc::new(a)
    }

    // go: PanicOnExceed.SetLogHook L1
    pub fn set_log_hook(&self, hook: Option<LogHook>) {
        *self.log_hook.lock() = hook;
    }
}

impl ActionOnExceed for PanicOnExceed {
    // go: PanicOnExceed.Action L2 (panic(err) → Err; a nil Killer still panics as in Go)
    fn action(&self, t: &Arc<Tracker>) -> Result<(), Error> {
        let mut acted = self.acted.lock();
        if !*acted {
            let hook = self.log_hook.lock().clone();
            match hook {
                None => {
                    let err = mem_exceed_error(t);
                    tracing::warn!(conn = t.session_id(), error = %err, "memory exceeds quota");
                }
                Some(h) => h(self.conn_id.load(SeqCst)),
            }
        }
        *acted = true;
        let killer = self
            .killer
            .as_ref()
            .expect("runtime error: invalid memory address or nil pointer dereference");
        killer.send_kill_signal(sqlkiller::QueryMemoryExceeded);
        let r = killer.handle_signal();
        drop(acted);
        r
    }
    // glue: Go embeds BaseOOMAction; forwarded to its SetFallback
    fn set_fallback(&self, a: Option<ActionRef>) {
        self.base.set_fallback(a)
    }
    // glue: Go embeds BaseOOMAction; forwarded to its GetFallback
    fn get_fallback(&self) -> Option<ActionRef> {
        self.base.get_fallback()
    }
    // go: PanicOnExceed.GetPriority L1
    fn get_priority(&self) -> i64 {
        DefPanicPriority
    }
    // glue: Go embeds BaseOOMAction; forwarded to its SetFinished
    fn set_finished(&self) {
        self.base.set_finished()
    }
    // glue: Go embeds BaseOOMAction; forwarded to its IsFinished
    fn is_finished(&self) -> bool {
        self.base.is_finished()
    }
}
