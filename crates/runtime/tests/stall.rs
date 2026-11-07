//! Poll-stall detection (N3 3.6). A separate test binary (process): it stalls on purpose, and
//! the stall count is process-wide.

use std::sync::Mutex;
use std::time::Duration;

use tidb_runtime::CheckpointStride;
use tidb_runtime::Runtime;
use tidb_runtime::RuntimeConfig;
use tidb_runtime::SliceConfig;
use tidb_runtime::Topology;
use tidb_runtime::checkpoint;
use tidb_runtime::slice::StallCheck;

/// The stall count is process-wide: these tests run one at a time.
static SERIAL: Mutex<()> = Mutex::new(());

#[test]
fn stall_detection() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
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

/// PR #8 review 2: Go's KILL checkpoints are per chunk (around `exec.Next`), and one `Next`
/// can take hundreds of µs. With a checkpoint every 400 µs of work, polls must stay around the
/// 1 ms slice: zero stalls at the default 10 ms threshold (with a clock read every 32 calls,
/// each poll was ~12.8 ms).
#[test]
fn chunk_level_checkpoints_do_not_stall() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    for topology in [Topology::Steal, Topology::NoSteal] {
        let rt = Runtime::new(RuntimeConfig {
            topology,
            worker_threads: 1,
            ..RuntimeConfig::default()
        })
        .unwrap();
        let pool = rt.pool().clone();
        let check = StallCheck::start();
        rt.block_on(async move {
            pool.spawn("test-next-400us", async {
                for _ in 0..200 {
                    checkpoint(&()).await.unwrap();
                    spin(Duration::from_micros(400));
                }
            })
            .await
            .unwrap();
        });
        assert_eq!(check.stalls(), 0, "{topology:?}");
    }
}

/// Per-row loops with a `CheckpointStride`: 64 rows of 20 µs between clock reads, ~1.3 ms per
/// poll at most.
#[test]
fn stride_checkpoints_do_not_stall() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let rt = Runtime::new(RuntimeConfig {
        worker_threads: 1,
        ..RuntimeConfig::default()
    })
    .unwrap();
    let pool = rt.pool().clone();
    let check = StallCheck::start();
    rt.block_on(async move {
        pool.spawn("test-stride", async {
            let mut cp = CheckpointStride::new(64);
            for _ in 0..5000 {
                cp.checkpoint(&()).await.unwrap();
                spin(Duration::from_micros(20));
            }
        })
        .await
        .unwrap();
    });
    assert_eq!(check.stalls(), 0);
}

fn spin(d: Duration) {
    let until = std::time::Instant::now() + d;
    while std::time::Instant::now() < until {
        std::hint::spin_loop();
    }
}

/// Classification (PR #9 review): a poll that spins is `on_cpu`; a poll that sleeps in a
/// blocking call is `blocked` on Linux (voluntary switch) and `off_cpu` where the platform has
/// no per-thread switch counts (macOS). Neither is `preempted`.
#[test]
fn stall_causes() {
    use tidb_runtime::stall::StallCause;
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let rt = Runtime::new(RuntimeConfig {
        worker_threads: 1,
        slice: SliceConfig {
            stall_threshold: Duration::from_millis(50),
            classify_stalls: true,
            ..SliceConfig::default()
        },
        ..RuntimeConfig::default()
    })
    .unwrap();
    let pool = rt.pool().clone();

    let check = StallCheck::start();
    let p = pool.clone();
    rt.block_on(async move {
        p.spawn("test-spin", async { spin(Duration::from_millis(100)) })
            .await
            .unwrap();
    });
    assert_eq!(check.stalls(), 1);
    assert_eq!(check.stalls_by(StallCause::OnCpu), 1);

    let check = StallCheck::start();
    rt.block_on(async move {
        pool.spawn("test-sleep", async {
            std::thread::sleep(Duration::from_millis(100));
        })
        .await
        .unwrap();
    });
    assert_eq!(check.stalls(), 1);
    let want = if cfg!(target_os = "linux") {
        StallCause::Blocked
    } else {
        StallCause::OffCpu
    };
    assert_eq!(check.stalls_by(want), 1, "{want:?}");
    assert_eq!(check.stalls_by(StallCause::OnCpu), 0);
}
