//! Structured log parity tests for `tidb-memory` (`meminfo`, `sqlkiller`, `action`).
//! Own integration test binary: installs the global `tidb_base::structured_log` test logger
//! and exercises `init_memory_hook`.

use std::fmt::Write as _;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use tidb_base::structured_log::{LogLevel, LogRecord, LogTarget, StructuredLogger};
use tidb_memory::action::{ActionOnExceed, LogOnExceed, PanicOnExceed};
use tidb_memory::meminfo;
use tidb_memory::sqlkiller::SqlKiller;
use tidb_memory::tracker::new_tracker;

#[derive(Clone, Default)]
struct Buf(Arc<Mutex<Vec<String>>>);

struct TestStructuredLogger(Buf);

impl StructuredLogger for TestStructuredLogger {
    fn enabled(&self, _target: LogTarget, _level: LogLevel) -> bool {
        true
    }
    fn level(&self) -> LogLevel {
        LogLevel::Debug
    }
    fn log(&self, record: &LogRecord<'_>) {
        let mut line = format!("[{}] [{}]", record.level.as_upper_str(), record.msg);
        for f in record.fields {
            let _ = write!(line, " [{}={:?}]", f.key, f.value);
        }
        self.0.0.lock().unwrap().push(line);
    }
    fn sync(&self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn memory_structured_logs_emit_go_messages_and_fields() {
    let buf = Buf::default();
    let logger: &'static TestStructuredLogger =
        Box::leak(Box::new(TestStructuredLogger(buf.clone())));
    let _ = tidb_base::structured_log::register_logger(logger);

    // 1. init_memory_hook emits one of the 3 Go INFO lines (possibly preceded by cgroup WARN
    //    when running outside a memory cgroup).
    meminfo::init_memory_hook();
    let init_lines: Vec<String> = buf.0.lock().unwrap().drain(..).collect();
    for line in &init_lines {
        let ok = line == "[WARN] [no cgroup memory controller detected]"
            || line == "[INFO] [use cgroup memory hook because TiDB is in the container]"
            || (line.starts_with("[INFO] [use cgroup memory hook] [cgroupMemorySize=I64(")
                && line.contains(")] [physicalMemorySize=I64("))
            || (line.starts_with("[INFO] [use physical memory hook] [cgroupMemorySize=I64(")
                && line.contains(")] [physicalMemorySize=I64("));
        assert!(ok, "unexpected init_memory_hook log line: {line}");
    }

    // 2. LogOnExceed emits Go WARN `"memory exceeds quota"` with `"error"` field.
    let t = new_tracker(1, 1);
    let log_action = LogOnExceed::new(42);
    log_action.action(&t).unwrap();
    let lines: Vec<String> = buf.0.lock().unwrap().drain(..).collect();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(
        lines[0].starts_with("[WARN] [memory exceeds quota] [error=Error("),
        "{}",
        lines[0]
    );

    // 3. PanicOnExceed + SqlKiller emit `"memory exceeds quota"` (with `conn`, `error`),
    //    `"kill initiated"` (with `"connection ID"`, `reason`), and `"kill finished"` (`conn`).
    let killer = SqlKiller::new();
    killer.conn_id.store(42, Ordering::SeqCst);
    let t2 = new_tracker(1, 1);
    t2.session_id.store(42, Ordering::SeqCst);
    let panic_action = PanicOnExceed::new(Some(Arc::clone(&killer)), 42);
    assert!(panic_action.action(&t2).is_err());
    killer.reset();

    let lines: Vec<String> = buf.0.lock().unwrap().drain(..).collect();
    assert_eq!(lines.len(), 3, "{lines:?}");
    assert!(
        lines[0].starts_with("[WARN] [memory exceeds quota] [conn=U64(42)] [error=Error("),
        "{}",
        lines[0]
    );
    assert!(
        lines[1].starts_with(
            "[WARN] [kill initiated] [connection ID=U64(42)] [reason=Str(\"[executor:8175]"
        ),
        "{}",
        lines[1]
    );
    assert_eq!(lines[2], "[WARN] [kill finished] [conn=U64(42)]");
}
