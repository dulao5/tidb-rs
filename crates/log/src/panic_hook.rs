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
    PanicMode, current_panic_mode, enter_panic_hook, panic_payload_message,
    store_recovered_backtrace,
};

use crate::encoder::text::safe_add_bytes;

/// Install the process-wide panic hook.
// glue: unified single-line panic hook
pub fn set_panic_hook() {
    std::panic::set_hook(Box::new(handle_panic));
}

fn handle_panic(info: &PanicHookInfo<'_>) {
    match current_panic_mode() {
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

            // Guard against re-entrant panic inside the logger (e.g. `log.timeout` firing while
            // logging an unhandled panic): fall back to a single-line escaped write on stderr.
            let Some(_reentry) = enter_panic_hook() else {
                let mut line = Vec::with_capacity(128);
                line.extend_from_slice(b"[FATAL] [\"unhandled panic (re-entrant)\"] [panic=\"");
                safe_add_bytes(&mut line, msg.as_bytes());
                line.extend_from_slice(b"\"] [location=\"");
                safe_add_bytes(&mut line, loc.as_bytes());
                line.extend_from_slice(b"\"]\n");
                let _ = io::stderr().write_all(&line);
                return;
            };

            let bt = format_backtrace_string(&Backtrace::force_capture());
            tidb_base::log_fatal!(
                "unhandled panic",
                "panic" => &msg,
                "location" => &loc,
                "stack" => &bt,
            );
            let _ = tidb_base::structured_log::logger().sync();
        }
    }
}
