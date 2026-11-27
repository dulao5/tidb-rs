//! Global single-line panic hook integrated with `tidb_base::panic_guard` (spec §2.3).
//!
//! Ensures unhandled panics emit a single-line `[FATAL] ... ["unhandled panic"]` record through
//! the unified encoder and sync sinks before process abort/exit, while recoverable boundaries
//! (`catch_unwind_capture` / `catch_unwind_future`) capture backtraces silently and value
//! boundaries (`catch_unwind_silent`) do zero work.

use std::backtrace::Backtrace;
use std::io::{self, Write};
use std::panic::PanicHookInfo;

use tidb_base::errors::format_backtrace_string;
use tidb_base::panic_guard::{
    PanicMode, current_panic_mode, enter_panic_hook, mark_scope_panic_and_check_double,
    panic_payload_message, store_recovered_backtrace,
};
use tidb_base::structured_log::{LogField, LogLevel, LogRecord, LogTarget, StructuredLogger};

use crate::encoder::text::safe_add_bytes;

/// Install the process-wide panic hook.
// glue: unified single-line panic hook
pub fn set_panic_hook() {
    std::panic::set_hook(Box::new(handle_panic));
}

fn write_stderr_fallback(msg: &str, loc: &str, reason: &str) {
    let mut line = Vec::with_capacity(160);
    line.extend_from_slice(b"[FATAL] [\"unhandled panic (");
    safe_add_bytes(&mut line, reason.as_bytes());
    line.extend_from_slice(b")\"] [panic=\"");
    safe_add_bytes(&mut line, msg.as_bytes());
    line.extend_from_slice(b"\"] [location=\"");
    safe_add_bytes(&mut line, loc.as_bytes());
    line.extend_from_slice(b"\"]\n");
    let _ = io::stderr().write_all(&line);
}

fn handle_panic(info: &PanicHookInfo<'_>) {
    // Non-unwinding panics (panic=abort builds, or a second panic inside Drop while already
    // unwinding a Silent/CaptureBacktrace scope) cannot be caught by catch_unwind and will abort
    // the process; always log them as FatalLog.
    let active = current_panic_mode();
    let mode = if cfg!(panic = "abort")
        || (active != PanicMode::FatalLog && mark_scope_panic_and_check_double())
    {
        PanicMode::FatalLog
    } else {
        active
    };

    match mode {
        PanicMode::Silent => {}
        PanicMode::CaptureBacktrace => {
            let bt = format_backtrace_string(&Backtrace::force_capture());
            store_recovered_backtrace(bt);
        }
        PanicMode::FatalLog => {
            let msg = panic_payload_message(info.payload());
            let loc = info
                .location()
                .map(|l| format!("{}:{}", l.file(), l.line()))
                .unwrap_or_else(|| "unknown:0".to_owned());

            // Mark this thread as inside the panic hook so sinks (e.g. LockWithTimeoutSink)
            // return Err instead of panicking (which would cause std to abort without calling
            // the hook again).
            let Some(_reentry) = enter_panic_hook() else {
                write_stderr_fallback(&msg, &loc, "re-entrant");
                return;
            };

            let bt = format_backtrace_string(&Backtrace::force_capture());
            let fields = [
                LogField::new("panic", msg.as_str()),
                LogField::new("location", loc.as_str()),
                LogField::new("stack", bt.as_str()),
            ];
            let record = LogRecord {
                target: LogTarget::Fatal,
                level: LogLevel::Fatal,
                logger_name: "",
                file: file!(),
                line: line!(),
                msg: "unhandled panic",
                fields: &fields,
            };
            let core = crate::logger::core();
            if core.try_emit_with_bound(&record, &[]).is_err() || core.sync().is_err() {
                write_stderr_fallback(&msg, &loc, "sink-fallback");
            }
        }
    }
}
