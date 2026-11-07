//! Poll-stall detection (N3 3.6). A separate test binary (process): it stalls on purpose, and
//! the stall count is process-wide.

use std::time::Duration;

use tidb_runtime::Runtime;
use tidb_runtime::RuntimeConfig;
use tidb_runtime::SliceConfig;
use tidb_runtime::Topology;
use tidb_runtime::checkpoint;
use tidb_runtime::slice::StallCheck;

#[test]
fn stall_detection() {
    for topology in [Topology::Steal, Topology::NoSteal] {
        let rt = Runtime::new(RuntimeConfig {
            topology,
            worker_threads: 1,
            // Above OS scheduling noise on a busy CI runner (the default is 10 ms).
            slice: SliceConfig {
                stall_threshold: Duration::from_millis(50),
                ..SliceConfig::default()
            },
            ..RuntimeConfig::default()
        })
        .unwrap();
        let pool = rt.pool().clone();

        // A loop with checkpoints never stalls: it yields every ~1 ms.
        let check = StallCheck::start();
        let p = pool.clone();
        rt.block_on(async move {
            p.spawn("test-sliced", async {
                let until = std::time::Instant::now() + Duration::from_millis(60);
                while std::time::Instant::now() < until {
                    checkpoint(&()).await.unwrap();
                }
            })
            .await
            .unwrap();
        });
        check.assert_none();

        // Blocking a runtime thread for 100 ms in one poll is a stall (M-BLOCK violation).
        let check = StallCheck::start();
        rt.block_on(async move {
            pool.spawn("test-blocking", async {
                std::thread::sleep(Duration::from_millis(100));
            })
            .await
            .unwrap();
        });
        assert_eq!(check.stalls(), 1, "{topology:?}");
    }
}
