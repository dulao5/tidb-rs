//! Scoped panic-hook controls and safe `catch_unwind` wrappers (rank 0, M-PANIC / M-DEFER).
//!
//! Placed in `tidb-base` (rank 0) so every layered crate (ranks 1..=16) can guard recoverable
//! boundaries without depending on `tidb-log` (rank 1). The global panic hook installed by
//! `tidb_log::set_panic_hook` consults [`current_panic_mode`] to decide whether a panic is
//! unhandled (`FatalLog`), recoverable with captured backtrace (`CaptureBacktrace`), or silent
//! value conversion (`Silent`).
//!
//! Async boundaries MUST use [`catch_unwind_future`], which mounts [`PanicScopeGuard`] only during
//! each synchronous `Future::poll` and drops it before returning `Poll::Pending` / `Poll::Ready`
//! so thread-local state never leaks across `.await` points.

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::future::Future;
use std::panic::{AssertUnwindSafe, UnwindSafe};
use std::pin::Pin;
use std::task::{Context, Poll};

/// Behavior of the process-wide panic hook on the current thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanicMode {
    /// Unhandled panic: log `[FATAL]` `"unhandled panic"` with single-line backtrace and sync.
    FatalLog,
    /// Recoverable boundary: suppress `[FATAL]` and capture `Backtrace::force_capture()` into the
    /// thread-local slot for the catching `Err(payload)` branch to consume.
    CaptureBacktrace,
    /// Value-conversion boundary (e.g. `safe_clone`, `append_datum_for_checksum`): suppress both
    /// `[FATAL]` output and backtrace capture.
    Silent,
}

thread_local! {
    static PANIC_MODE: Cell<PanicMode> = const { Cell::new(PanicMode::FatalLog) };
    static SCOPE_PANIC_SEEN: Cell<bool> = const { Cell::new(false) };
    static LAST_RECOVERED_BACKTRACE: RefCell<Option<String>> = const { RefCell::new(None) };
    static IN_PANIC_HOOK: Cell<bool> = const { Cell::new(false) };
}

/// RAII scope guard that restores the previous [`PanicMode`] on drop (nesting-safe, M-DEFER).
pub struct PanicScopeGuard {
    prev: PanicMode,
    prev_seen: bool,
}

impl Drop for PanicScopeGuard {
    // glue: restore outer PanicMode on normal exit or unwind (M-DEFER)
    fn drop(&mut self) {
        PANIC_MODE.with(|cell| cell.set(self.prev));
        SCOPE_PANIC_SEEN.with(|cell| cell.set(self.prev_seen));
    }
}

/// Enter `mode` on the current thread until the returned [`PanicScopeGuard`] is dropped.
// glue: nested-safe TLS panic scope
pub fn enter_panic_mode(mode: PanicMode) -> PanicScopeGuard {
    let prev = PANIC_MODE.with(|cell| cell.replace(mode));
    let prev_seen = SCOPE_PANIC_SEEN.with(|cell| cell.replace(false));
    PanicScopeGuard { prev, prev_seen }
}

/// Record that the current recoverable scope has entered a panic unwind.
///
/// Returns `true` if a panic was ALREADY seen in this active scope (a double panic inside
/// `Drop` during unwinding, which cannot be caught and will abort the process).
// glue: stable-Rust detection of non-unwinding double-panic inside Silent/CaptureBacktrace scopes
pub fn mark_scope_panic_and_check_double() -> bool {
    SCOPE_PANIC_SEEN.with(|cell| cell.replace(true))
}

/// Read the active [`PanicMode`] on the current thread.
// glue: queried by tidb_log global panic hook
pub fn current_panic_mode() -> PanicMode {
    PANIC_MODE.with(Cell::get)
}

/// Store a captured backtrace from the panic hook when [`PanicMode::CaptureBacktrace`] is active.
// glue: called by tidb_log global panic hook
pub fn store_recovered_backtrace(bt: String) {
    LAST_RECOVERED_BACKTRACE.with(|slot| {
        *slot.borrow_mut() = Some(bt);
    });
}

/// Take the captured backtrace stored during an unwind.
///
/// Callers MUST only invoke this in the `Err(payload)` branch of a `catch_unwind` — never on
/// `Ok(Err(...))` normal error returns.
// glue: consume captured panic backtrace in Err(payload) branch
pub fn take_recovered_backtrace() -> Option<String> {
    LAST_RECOVERED_BACKTRACE.with(|slot| slot.borrow_mut().take())
}

/// Clear any stale recovered backtrace slot on the current thread.
fn clear_recovered_backtrace() {
    LAST_RECOVERED_BACKTRACE.with(|slot| {
        *slot.borrow_mut() = None;
    });
}

/// Re-entrancy guard for the global panic hook (prevents infinite recursion if logging itself
/// panics, e.g. under `log.timeout`).
pub struct PanicHookReentryGuard;

impl Drop for PanicHookReentryGuard {
    // glue: clear IN_PANIC_HOOK on drop
    fn drop(&mut self) {
        IN_PANIC_HOOK.with(|cell| cell.set(false));
    }
}

/// Attempt to enter the global panic hook. Returns `None` if already inside the panic hook on this
/// thread (re-entrant panic).
// glue: re-entrancy protection for tidb_log panic hook
pub fn enter_panic_hook() -> Option<PanicHookReentryGuard> {
    IN_PANIC_HOOK.with(|cell| {
        if cell.get() {
            None
        } else {
            cell.set(true);
            Some(PanicHookReentryGuard)
        }
    })
}

/// Returns `true` while the current thread is executing inside the global panic hook.
///
/// Sinks check this flag to avoid panicking (e.g. on `log.timeout`) while the panic hook is
/// writing its `[FATAL]` record — panicking inside a panic hook causes Rust's `std` to abort
/// immediately without re-invoking the hook.
// glue: sink non-panicking guard while inside panic hook
pub fn is_in_panic_hook() -> bool {
    IN_PANIC_HOOK.with(Cell::get)
}

/// Extract a human-readable message from a caught panic payload.
// glue: panic payload string extraction
pub fn panic_payload_message(payload: &(dyn Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_owned()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "panic".to_owned()
    }
}

/// Run `f` under [`PanicMode::Silent`] and catch any unwind without logging or capturing a
/// backtrace (e.g. `safe_clone`, `append_datum_for_checksum`).
// glue: M-PANIC silent value-conversion catch_unwind
pub fn catch_unwind_silent<F, R>(f: F) -> std::thread::Result<R>
where
    F: FnOnce() -> R + UnwindSafe,
{
    let _guard = enter_panic_mode(PanicMode::Silent);
    // log-skip: sanctioned panic_guard wrapper
    std::panic::catch_unwind(f)
}

/// Run `f` under [`PanicMode::CaptureBacktrace`] and catch any unwind, returning the panic payload
/// and the captured backtrace (if any) only on unwind.
// glue: M-PANIC recoverable boundary catch_unwind with backtrace capture
pub fn catch_unwind_capture<F, R>(f: F) -> Result<R, (Box<dyn Any + Send>, Option<String>)>
where
    F: FnOnce() -> R + UnwindSafe,
{
    clear_recovered_backtrace();
    let _guard = enter_panic_mode(PanicMode::CaptureBacktrace);
    // log-skip: sanctioned panic_guard wrapper
    match std::panic::catch_unwind(f) {
        Ok(v) => Ok(v),
        Err(payload) => Err((payload, take_recovered_backtrace())),
    }
}

/// Future wrapper that catches panics during each `poll` under [`PanicMode::CaptureBacktrace`]
/// without leaking thread-local panic mode across `.await` suspension points.
pub struct CatchUnwindFuture<F> {
    inner: Pin<Box<F>>,
}

impl<F: Future> Future for CatchUnwindFuture<F> {
    type Output = Result<F::Output, (Box<dyn Any + Send>, Option<String>)>;

    // glue: mount CaptureBacktrace strictly around inner.poll(cx)
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        clear_recovered_backtrace();
        let _guard = enter_panic_mode(PanicMode::CaptureBacktrace);
        // log-skip: sanctioned panic_guard async wrapper
        match std::panic::catch_unwind(AssertUnwindSafe(|| self.inner.as_mut().poll(cx))) {
            Ok(Poll::Pending) => Poll::Pending,
            Ok(Poll::Ready(v)) => Poll::Ready(Ok(v)),
            Err(payload) => Poll::Ready(Err((payload, take_recovered_backtrace()))),
        }
    }
}

/// Wrap `fut` so any panic during polling is caught with its backtrace captured, without leaking
/// thread-local state across `.await` boundaries.
// glue: M-PANIC async recoverable boundary wrapper
pub fn catch_unwind_future<F: Future>(fut: F) -> CatchUnwindFuture<F> {
    CatchUnwindFuture {
        inner: Box::pin(fut),
    }
}
