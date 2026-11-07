//! The runtime's warnings reach a `tracing` subscriber such as tidb-server's (PR #10 review:
//! they were `log` records, which the server's subscriber never saw). Own test binary: it
//! installs the global subscriber.

use std::io::Write;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use tidb_runtime::Runtime;
use tidb_runtime::RuntimeConfig;
use tidb_runtime::SliceConfig;

#[derive(Clone, Default)]
struct Buf(Arc<Mutex<Vec<u8>>>);

impl Write for Buf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn stall_and_panic_are_logged_through_tracing() {
    let buf = Buf::default();
    let w = buf.clone();
    tracing_subscriber::fmt()
        .with_writer(move || w.clone())
        .with_ansi(false)
        .init();

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
    assert!(
        out.contains("poll stall") && out.contains("test-log-stall"),
        "{out}"
    );
    assert!(
        out.contains("panic in the recoverable goroutine") && out.contains("boom for the log"),
        "{out}"
    );
}
