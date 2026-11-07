//! Runtime tests. The ones that run tasks run on both topologies (N3 §5 "两种拓扑").
//! Stall detection is in `tests/stall.rs`, a separate process, because it stalls on purpose.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use tidb_runtime::Admission;
use tidb_runtime::AdmissionConfig;
use tidb_runtime::BlockingConfig;
use tidb_runtime::BlockingKind;
use tidb_runtime::BlockingPool;
use tidb_runtime::NotSupported;
use tidb_runtime::Runtime;
use tidb_runtime::RuntimeConfig;
use tidb_runtime::StoreWindows;
use tidb_runtime::StoreWindowsConfig;
use tidb_runtime::TaskExit;
use tidb_runtime::TokenLimiter;
use tidb_runtime::Topology;
use tidb_runtime::checkpoint;
use tidb_runtime::slice;
use tidb_runtime::slice::CpuRecorder;

fn runtimes(threads: usize) -> Vec<Runtime> {
    [Topology::Steal, Topology::NoSteal]
        .into_iter()
        .map(|topology| {
            Runtime::new(RuntimeConfig {
                topology,
                worker_threads: threads,
                ..RuntimeConfig::default()
            })
            .expect("runtime")
        })
        .collect()
}

// ------------------------------------------------------------------------------------- Q1

/// Go: goroutines blocked receiving from the token channel get tokens in FIFO order.
#[tokio::test]
async fn token_limiter_fifo() {
    let tl = TokenLimiter::new(1);
    let held = tl.get().await;
    let order = Arc::new(Mutex::new(Vec::new()));
    let mut hs = Vec::new();
    for i in 0..5 {
        let tl = tl.clone();
        let order = order.clone();
        hs.push(tokio::spawn(async move {
            let tk = tl.get().await;
            order.lock().unwrap().push(i);
            tl.put(tk);
        }));
        // current-thread runtime: let waiter i queue up before spawning i+1.
        tokio::task::yield_now().await;
    }
    tl.put(held);
    for h in hs {
        h.await.unwrap();
    }
    assert_eq!(*order.lock().unwrap(), vec![0, 1, 2, 3, 4]);
    assert_eq!(tl.available(), 1);
    assert_eq!(tl.count(), 1);
}

/// Go never rejects: with all tokens taken, `Get` waits (here: until the timeout).
#[tokio::test(start_paused = true)]
async fn token_limiter_waits_never_rejects() {
    let adm = Admission::new(&AdmissionConfig {
        token_limit: 2,
        ..AdmissionConfig::default()
    })
    .unwrap();
    let a = adm.get().await;
    let _b = adm.get().await;
    assert!(
        tokio::time::timeout(Duration::from_secs(3600), adm.get())
            .await
            .is_err()
    );
    adm.put(a);
    let _c = tokio::time::timeout(Duration::from_secs(1), adm.get())
        .await
        .expect("a token is free again");
    assert_eq!(adm.limiter().available(), 0);
}

#[test]
fn admission_exceptions_are_off() {
    assert_eq!(AdmissionConfig::default().token_limit, 1000);
    assert!(matches!(
        Admission::new(&AdmissionConfig {
            per_group_shares: true,
            ..AdmissionConfig::default()
        }),
        Err(NotSupported(_))
    ));
    assert!(
        Admission::new(&AdmissionConfig {
            reject_after: Some(Duration::from_secs(1)),
            ..AdmissionConfig::default()
        })
        .is_err()
    );
}

// ------------------------------------------------------------------------------------- Q2

#[tokio::test]
async fn store_windows_default_is_unlimited() {
    let w = StoreWindows::new(&StoreWindowsConfig::default()).unwrap();
    let mut ps = Vec::new();
    for _ in 0..5000 {
        ps.push(w.acquire(1, "s1:20160").await.unwrap());
    }
    assert_eq!(w.inflight(1), 5000);
    assert_eq!(w.inflight(2), 0);
    drop(ps);
    assert_eq!(w.inflight(1), 0);
}

/// client-go getStoreToken: fails (does not wait) when `limit` requests are in flight.
#[tokio::test]
async fn store_limit_fails_like_client_go() {
    let w = StoreWindows::new(&StoreWindowsConfig {
        store_limit: 2,
        ..StoreWindowsConfig::default()
    })
    .unwrap();
    let a = w.acquire(7, "s7:20160").await.unwrap();
    let _b = w.acquire(7, "s7:20160").await.unwrap();
    let e = w.acquire(7, "s7:20160").await.err().expect("limit reached");
    assert_eq!(e.store_id, 7);
    assert_eq!(
        e.to_string(),
        "Store token is up to the limit, store id = 7."
    );
    // Other stores have their own count.
    let _other = w.acquire(8, "s8:20160").await.unwrap();
    drop(a);
    let _c = w.acquire(7, "s7:20160").await.unwrap();
    // Turning the limit off (0) lets everything through again.
    w.set_store_limit(0);
    let _d = w.acquire(7, "s7:20160").await.unwrap();
    assert_eq!(w.inflight(7), 3);
}

/// The Rust-only window makes requests wait instead of failing.
#[tokio::test(start_paused = true)]
async fn store_window_waits() {
    let w = Arc::new(
        StoreWindows::new(&StoreWindowsConfig {
            window: Some(1),
            ..StoreWindowsConfig::default()
        })
        .unwrap(),
    );
    let a = w.acquire(1, "s1").await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(60), w.acquire(1, "s1"))
            .await
            .is_err()
    );
    // A different store is not affected.
    let _other = w.acquire(2, "s2").await.unwrap();
    let w2 = w.clone();
    let waiter = tokio::spawn(async move { w2.acquire(1, "s1").await.map(|_| ()) });
    tokio::task::yield_now().await;
    drop(a);
    waiter.await.unwrap().unwrap();
    w.observe_schedule_wait(1, Duration::from_micros(250));
}

#[test]
fn store_window_exceptions_are_off() {
    for cfg in [
        StoreWindowsConfig {
            aimd: true,
            ..StoreWindowsConfig::default()
        },
        StoreWindowsConfig {
            group_shares: true,
            ..StoreWindowsConfig::default()
        },
        StoreWindowsConfig {
            window: Some(0),
            ..StoreWindowsConfig::default()
        },
    ] {
        assert!(StoreWindows::new(&cfg).is_err());
    }
}

// ---------------------------------------------------------------------- checkpoint / slice

/// A busy loop that calls `checkpoint` lets another task on the same (single) worker run;
/// without the time slice the second task could only run after the loop ends.
#[test]
fn checkpoint_yields_busy_loop() {
    for rt in runtimes(1) {
        let topo = rt.pool().topology();
        let pool = rt.pool().clone();
        let ok = rt.block_on(async move {
            let flag = Arc::new(AtomicBool::new(false));
            let f2 = flag.clone();
            let busy = pool.spawn("test-busy", async move {
                let deadline = Instant::now() + Duration::from_secs(5);
                let mut n = 0u64;
                while !f2.load(Ordering::Relaxed) && Instant::now() < deadline {
                    checkpoint(&()).await.unwrap();
                    n = n.wrapping_add(1);
                    std::hint::black_box(n);
                }
                f2.load(Ordering::Relaxed)
            });
            let f3 = flag.clone();
            let other = pool.spawn("test-other", async move {
                f3.store(true, Ordering::Relaxed);
            });
            other.await.unwrap();
            busy.await.unwrap()
        });
        assert!(ok, "{topo:?}: the busy loop never yielded");
    }
}

/// The kill / memory hook runs on every call and its error stops the loop.
#[test]
fn checkpoint_runs_hook_every_call() {
    struct Killer(AtomicUsize);
    impl tidb_runtime::CheckpointHook for Killer {
        type Error = &'static str;
        fn check(&self) -> Result<(), &'static str> {
            if self.0.fetch_add(1, Ordering::Relaxed) == 9 {
                Err("Query execution was interrupted")
            } else {
                Ok(())
            }
        }
    }
    for rt in runtimes(2) {
        let pool = rt.pool().clone();
        let (calls, err) = rt.block_on(async move {
            pool.spawn("test-kill", async move {
                let k = Killer(AtomicUsize::new(0));
                let mut err = None;
                for _ in 0..100 {
                    if let Err(e) = checkpoint(&k).await {
                        err = Some(e);
                        break;
                    }
                }
                (k.0.load(Ordering::Relaxed), err)
            })
            .await
            .unwrap()
        });
        assert_eq!(calls, 10);
        assert_eq!(err, Some("Query execution was interrupted"));
    }
}

/// CPU time is attributed to the attached recorder, including spawned workers'.
#[test]
fn cpu_recorder_inherited() {
    for rt in runtimes(2) {
        let pool = rt.pool().clone();
        let total = rt.block_on(async move {
            let r = CpuRecorder::new();
            slice::set_cpu_recorder(Some(r.clone()));
            let worker = pool.spawn("test-cpu-worker", async {
                let until = Instant::now() + Duration::from_millis(20);
                while Instant::now() < until {
                    checkpoint(&()).await.unwrap();
                }
            });
            worker.await.unwrap();
            assert!(slice::task_cpu_time().is_some());
            r.total()
        });
        // The worker spins 20 ms of wall time; its polls cover nearly all of it.
        assert!(total >= Duration::from_millis(15), "{total:?}");
    }
}

/// NoSteal: a task spawned from a connection stays on the connection's thread.
#[test]
fn no_steal_spawn_stays_on_thread() {
    let rt = Runtime::new(RuntimeConfig {
        topology: Topology::NoSteal,
        worker_threads: 4,
        ..RuntimeConfig::default()
    })
    .unwrap();
    let pool = rt.pool().clone();
    for _ in 0..8 {
        let pool = pool.clone();
        let (a, b) = rt.block_on(async move {
            let here = std::thread::current().id();
            let there = pool
                .spawn("test-local", async { std::thread::current().id() })
                .await
                .unwrap();
            (here, there)
        });
        assert_eq!(a, b);
    }
}

// --------------------------------------------------------------------------- recover

#[test]
fn spawn_with_recover_like_go_with_recovery() {
    for rt in runtimes(2) {
        let pool = rt.pool().clone();
        rt.block_on(async move {
            // Panic: on_exit gets the panic, before the waiter sees the result; nothing
            // propagates.
            let seen = Arc::new(Mutex::new(Vec::new()));
            let s2 = seen.clone();
            let h = pool.spawn_with_recover(
                "test-panic",
                async {
                    tokio::task::yield_now().await;
                    if std::hint::black_box(true) {
                        panic!("boom");
                    }
                    1
                },
                move |e| s2.lock().unwrap().push(e),
            );
            assert_eq!(h.await.unwrap(), None);
            let got = std::mem::take(&mut *seen.lock().unwrap());
            assert_eq!(got.len(), 1);
            match &got[0] {
                TaskExit::Panicked(p) => assert_eq!(p.message, "boom"),
                other => panic!("{other:?}"),
            }

            // No panic: on_exit(Completed) once, result passed through.
            let s3 = seen.clone();
            let h = pool
                .spawn_with_recover("test-ok", async { 7 }, move |e| s3.lock().unwrap().push(e));
            assert_eq!(h.await.unwrap(), Some(7));
            assert_eq!(*seen.lock().unwrap(), vec![TaskExit::Completed]);
        });
    }
}

/// Go's recoverFn always runs; a task can be cancelled, and on_exit must still run (once).
#[test]
fn spawn_with_recover_on_abort() {
    for rt in runtimes(2) {
        let topo = rt.pool().topology();
        let pool = rt.pool().clone();
        let exits = rt.block_on(async move {
            let seen = Arc::new(Mutex::new(Vec::new()));
            // Aborted while running (sleeping).
            let s1 = seen.clone();
            let h = pool.spawn_with_recover(
                "test-abort",
                tokio::time::sleep(Duration::from_secs(3600)),
                move |e| s1.lock().unwrap().push(e),
            );
            tokio::task::yield_now().await;
            h.abort();
            assert!(h.await.unwrap_err().is_cancelled());
            // Aborted right after spawning, without yielding: on NoSteal the task (same
            // thread) cannot have run yet, so this is the "cancelled before the first poll"
            // case; on Steal another worker may have completed it first.
            let s2 = seen.clone();
            let h = pool.spawn_with_recover("test-abort-early", async { 1 }, move |e| {
                s2.lock().unwrap().push(e)
            });
            h.abort();
            let _ = h.await;
            std::mem::take(&mut *seen.lock().unwrap())
        });
        assert_eq!(exits.len(), 2, "{exits:?}");
        assert_eq!(exits[0], TaskExit::Cancelled);
        if topo == Topology::NoSteal {
            assert_eq!(exits[1], TaskExit::Cancelled);
        } else {
            assert!(matches!(
                exits[1],
                TaskExit::Cancelled | TaskExit::Completed
            ));
        }
    }
}

/// Dropping the runtime cancels its tasks; their on_exit runs.
#[test]
fn spawn_with_recover_on_runtime_drop() {
    for rt in runtimes(2) {
        let (tx, rx) = std::sync::mpsc::channel();
        let pool = rt.pool().clone();
        let started = rt.block_on(async move {
            let (stx, srx) = tokio::sync::oneshot::channel();
            let _h = pool.spawn_with_recover(
                "test-shutdown",
                async move {
                    let _ = stx.send(());
                    tokio::time::sleep(Duration::from_secs(3600)).await;
                },
                move |e| {
                    let _ = tx.send(e);
                },
            );
            srx.await.is_ok()
        });
        assert!(started);
        drop(rt);
        let e = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("on_exit ran at shutdown");
        assert_eq!(e, TaskExit::Cancelled);
    }
}

// ------------------------------------------------------------------------------------- Q3

#[test]
fn blocking_pool_is_bounded() {
    for rt in runtimes(2) {
        rt.block_on(async {
            let pool = BlockingPool::new(BlockingConfig { spill: 2, other: 1 });
            let now = Arc::new(AtomicUsize::new(0));
            let max = Arc::new(AtomicUsize::new(0));
            let mut hs = Vec::new();
            for _ in 0..8 {
                let pool = pool.clone();
                let now = now.clone();
                let max = max.clone();
                hs.push(tokio::spawn(async move {
                    pool.spawn_blocking(BlockingKind::Spill, move || {
                        let n = now.fetch_add(1, Ordering::SeqCst) + 1;
                        max.fetch_max(n, Ordering::SeqCst);
                        std::thread::sleep(Duration::from_millis(5));
                        now.fetch_sub(1, Ordering::SeqCst);
                    })
                    .await
                    .unwrap();
                }));
            }
            for h in hs {
                h.await.unwrap();
            }
            assert!(max.load(Ordering::SeqCst) <= 2);
            assert_eq!(pool.available(BlockingKind::Spill), 2);
            // A panic in the closure comes back as an error and frees the slot.
            let e = pool
                .spawn_blocking(BlockingKind::Spill, || {
                    if std::hint::black_box(true) {
                        panic!("blocking boom");
                    }
                })
                .await;
            assert!(e.is_err());
            assert_eq!(pool.available(BlockingKind::Spill), 2);

            // Categories do not starve each other: with `Other` fully taken by a long call,
            // a spill still runs.
            let (tx, rx) = std::sync::mpsc::channel::<()>();
            let p2 = pool.clone();
            let hog = tokio::spawn(async move {
                p2.spawn_blocking(BlockingKind::Other, move || {
                    let _ = rx.recv_timeout(Duration::from_secs(10));
                })
                .await
                .unwrap();
            });
            while pool.available(BlockingKind::Other) != 0 {
                tokio::task::yield_now().await;
            }
            let r = tokio::time::timeout(
                Duration::from_secs(5),
                pool.spawn_blocking(BlockingKind::Spill, || 42),
            )
            .await
            .expect("spill not starved")
            .unwrap();
            assert_eq!(r, 42);
            tx.send(()).unwrap();
            hog.await.unwrap();
        });
    }
}

// ----------------------------------------------------------------------------- metrics

#[test]
fn live_task_gauge_returns_to_zero() {
    tidb_runtime::metrics::init();
    for rt in runtimes(2) {
        let pool = rt.pool().clone();
        rt.block_on(async move {
            let hs: Vec<_> = (0..16)
                .map(|i| pool.spawn("test-gauge", async move { i }))
                .collect();
            for h in hs {
                h.await.unwrap();
            }
        });
        assert_eq!(tidb_runtime::metrics::live_tasks("test-gauge"), 0);
        // Exported through the custom collector.
        let fams = prometheus::gather();
        let tasks = fams
            .iter()
            .find(|f| f.get_name() == "tidb_rs_runtime_tasks")
            .expect("tasks gauge");
        let g = tasks
            .get_metric()
            .iter()
            .find(|m| m.get_label().iter().any(|l| l.get_value() == "test-gauge"))
            .expect("series");
        assert_eq!(g.get_gauge().get_value(), 0.0);
    }
}
