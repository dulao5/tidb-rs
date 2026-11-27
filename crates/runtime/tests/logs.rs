//! The runtime's structured warnings and panic recovery logs reach `tidb_base::structured_log`
//! directly (D00.3 Batch 1). Own test binary: it installs the global structured logger and a
//! `CaptureBacktrace` panic hook (mirrors `tidb_log::set_panic_hook`; full `tidb-log` text-encoder
//! whole-line parity is covered by `crates/types/tests/runtime_log_parity.rs` since `tidb-runtime`
//! and `tidb-log` share rank 1).

use std::backtrace::Backtrace;
use std::io::Write;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use tidb_base::errors::format_backtrace_string;
use tidb_base::panic_guard::{PanicMode, current_panic_mode, store_recovered_backtrace};
use tidb_base::structured_log::{LogLevel, LogRecord, LogTarget, StructuredLogger};
use tidb_runtime::Runtime;
use tidb_runtime::RuntimeConfig;
use tidb_runtime::SliceConfig;

#[derive(Clone, Default)]
struct Buf(Arc<Mutex<Vec<u8>>>);

struct TestStructuredLogger(Buf);

impl StructuredLogger for TestStructuredLogger {
    fn enabled(&self, _target: LogTarget, _level: LogLevel) -> bool {
        true
    }
    fn level(&self) -> LogLevel {
        LogLevel::Debug
    }
    fn log(&self, record: &LogRecord<'_>) {
        let mut g = self.0.0.lock().unwrap();
        let _ = write!(g, "[{}] [{}]", record.level.as_upper_str(), record.msg);
        for f in record.fields {
            let _ = write!(g, " [{}={:?}]", f.key, f.value);
        }
        let _ = writeln!(g);
    }
    fn sync(&self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn stall_and_panic_are_logged_through_structured_log() {
    let buf = Buf::default();
    let structured: &'static TestStructuredLogger =
        Box::leak(Box::new(TestStructuredLogger(buf.clone())));
    let _ = tidb_base::structured_log::register_logger(structured);
    std::panic::set_hook(Box::new(|_| {
        if current_panic_mode() == PanicMode::CaptureBacktrace {
            store_recovered_backtrace(format_backtrace_string(&Backtrace::force_capture()));
        }
    }));

    let rt = Runtime::new(RuntimeConfig {
        worker_threads: 1,
        slice: SliceConfig {
            stall_threshold: Duration::from_millis(20),
            classify_stalls: true,
            ..SliceConfig::default()
        },
        ..RuntimeConfig::default()
    })
    .unwrap();
    let pool = rt.pool().clone();
    rt.block_on(async move {
        pool.spawn("test-log-stall", async {
            std::thread::sleep(Duration::from_millis(60));
        })
        .await
        .unwrap();
        let h = pool.spawn_with_recover(
            "test-log-panic",
            async {
                if std::hint::black_box(true) {
                    panic!("boom for the log");
                }
            },
            |_| {},
        );
        assert_eq!(h.await.unwrap(), None);
    });
    let out = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 2, "expected 2 structured log lines:\n{out}");
    assert!(
        lines[0].starts_with("[WARN] [poll stall: a task ran without yielding] [cause=Str(")
            && lines[0].contains("[task=Str(\"test-log-stall\")] [elapsed=Duration(")
            && lines[0].ends_with("[threshold=Duration(20000000)]"),
        "unexpected stall log line: {}",
        lines[0]
    );
    let prefix = "[ERROR] [panic in the recoverable goroutine] [r=Str(\"boom for the log\")] [stack trace=Str(\"";
    let stack_body = lines[1]
        .strip_prefix(prefix)
        .and_then(|s| s.strip_suffix("\")]"))
        .unwrap_or_else(|| panic!("unexpected panic log line: {}", lines[1]));
    assert!(
        !stack_body.is_empty() && stack_body.contains("\\n"),
        "expected non-empty captured stack trace: {stack_body:?}"
    );
}
