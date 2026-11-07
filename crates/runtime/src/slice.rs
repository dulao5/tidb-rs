//! Per-task poll instrumentation (N3 3.1 第 5 条, 3.6 "poll 停顿检测"): the time-sliced
//! [`checkpoint`], CPU accounting, and poll-stall detection.
//!
//! Rust has no Go sysmon preemption. Two things replace it:
//!
//! - **Time-sliced yielding.** [`checkpoint`] sits where Go checks `HandleSQLKillerSignal`
//!   (mapping rule M-KILL). Every call runs the caller's kill / memory hook and reads the
//!   clock (~30 ns), and it yields when the current poll has run longer than `quantum`
//!   (default 1 ms). Go's KILL checkpoints are per chunk (`exec.Next`, internal/exec/
//!   executor.go:448/462; one `Next` can take hundreds of µs), so the clock is read on every
//!   call by default. Per-row inner loops (hash build, sort; M-KILL) use a
//!   [`CheckpointStride`] that reads the clock every `n` calls, like TiKV's
//!   `RescheduleChecker` (`components/tidb_query_common/src/storage/scanner.rs`: every 32
//!   keys, 1 ms).
//! - **Stall detection.** Every task started through the [`ExecPool`](crate::ExecPool) is
//!   wrapped so that each `poll` is timed. A poll longer than `stall_threshold` (default
//!   10 ms) is a stall: a loop that is missing a checkpoint, or blocking code (M-BLOCK). It is
//!   counted (metric and [`stall_count`]) and logged. N3 §5 requires zero stalls in tests and
//!   benchmarks; [`StallCheck`] asserts that.
//!
//! The time of each poll is the task's CPU time (tasks must not block), so the same wrapper
//! does CPU accounting: [`set_cpu_recorder`] attaches a [`CpuRecorder`] (a statement's, later
//! its resource group's) to the current task; tasks spawned from it inherit it.

use std::cell::Cell;
use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::task::Context;
use std::task::Poll;
use std::time::Duration;
use std::time::Instant;

use crate::metrics;
use crate::metrics::TaskMetrics;

/// Time-slice and stall parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SliceConfig {
    /// Yield at a checkpoint once the current poll has run this long. TiKV: 1 ms.
    pub quantum: Duration,
    /// Read the clock every this many [`checkpoint`] calls. Default 1 (Go's checkpoints are
    /// per chunk); per-row loops use [`CheckpointStride`] instead of raising this.
    pub check_every: u32,
    /// A poll longer than this is a stall. N3 3.6: 10 ms.
    pub stall_threshold: Duration,
    /// Classify stalls by thread CPU time and context switches ([`crate::stall`]): ~400 ns
    /// per poll, so off by default; on in tests and benchmarks (or with
    /// `TIDB_RS_STALL_CLASSIFY=1`).
    pub classify_stalls: bool,
}

impl Default for SliceConfig {
    // glue: N3 defaults
    fn default() -> SliceConfig {
        SliceConfig {
            quantum: Duration::from_millis(1),
            check_every: 1,
            stall_threshold: Duration::from_millis(10),
            classify_stalls: crate::stall::enabled_by_env(),
        }
    }
}

/// CPU time accumulated by the tasks it is attached to (a statement; later a resource group,
/// N3 3.1 第 5 条). Cheap to clone; an atomic add per poll.
///
/// It sums the polls' **wall** time: on an oversubscribed machine that includes time the
/// thread was preempted. Fine for the PoC; Top SQL / resource control will need thread CPU
/// time (see [`crate::stall`]) or a correction.
#[derive(Debug, Clone, Default)]
pub struct CpuRecorder(Arc<AtomicU64>);

impl CpuRecorder {
    // glue: CPU accounting
    pub fn new() -> CpuRecorder {
        CpuRecorder::default()
    }

    // glue: CPU accounting
    pub fn total(&self) -> Duration {
        Duration::from_nanos(self.0.load(Ordering::Relaxed))
    }

    // glue: CPU accounting
    fn add(&self, d: Duration) {
        let ns = u64::try_from(d.as_nanos()).unwrap_or(u64::MAX);
        self.0.fetch_add(ns, Ordering::Relaxed);
    }
}

/// State of one instrumented task, in a task-local (it follows the task across threads in the
/// Steal topology).
struct TaskState {
    cfg: SliceConfig,
    poll_start: Cell<Instant>,
    calls: Cell<u32>,
    cpu: Cell<Duration>,
    recorder: RefCell<Option<CpuRecorder>>,
}

tokio::task_local! {
    static TASK: TaskState;
}

static STALLS: AtomicU64 = AtomicU64::new(0);

/// Process-wide number of stalled polls so far.
// glue: N3 3.6 stall detection
pub fn stall_count() -> u64 {
    STALLS.load(Ordering::Relaxed)
}

/// Test helper: no task may stall between [`StallCheck::start`] and [`StallCheck::assert_none`]
/// (N3 §5 "停顿检测": zero stalls). Process-wide, so tests that use it must not run tasks that
/// stall on purpose in parallel.
pub struct StallCheck(u64, [u64; 5]);

impl StallCheck {
    // glue: test helper
    pub fn start() -> StallCheck {
        StallCheck(
            stall_count(),
            crate::stall::StallCause::ALL.map(crate::stall::stall_count_by),
        )
    }

    /// Stalls of cause `c` since [`StallCheck::start`].
    // glue: test helper
    pub fn stalls_by(&self, c: crate::stall::StallCause) -> u64 {
        let i = crate::stall::StallCause::ALL
            .iter()
            .position(|x| *x == c)
            .unwrap_or(0);
        crate::stall::stall_count_by(c) - self.1[i]
    }

    /// N3 §5: no stall that is the code's fault (`on_cpu`, `blocked`); `preempted` and
    /// `off_cpu` ones (machine load) are allowed. Needs `classify_stalls` on: unclassified
    /// stalls count as the code's fault.
    // glue: test helper
    #[track_caller]
    pub fn assert_no_code_stalls(&self) {
        use crate::stall::StallCause;
        let bad: u64 = [
            StallCause::OnCpu,
            StallCause::Blocked,
            StallCause::Unclassified,
        ]
        .into_iter()
        .map(|c| self.stalls_by(c))
        .sum();
        assert_eq!(
            bad, 0,
            "{bad} on-CPU / blocked / unclassified poll stall(s)"
        );
    }

    // glue: test helper
    pub fn stalls(&self) -> u64 {
        stall_count() - self.0
    }

    // glue: test helper
    #[track_caller]
    pub fn assert_none(&self) {
        let n = self.stalls();
        assert_eq!(
            n, 0,
            "{n} poll stall(s) (a poll ran longer than the stall threshold)"
        );
    }
}

/// Wraps a task's future: times each poll (CPU accounting, stall detection) and resets the
/// time slice. Created by [`instrument`].
pub(crate) struct Instrumented<F> {
    inner: Pin<Box<F>>,
    metrics: &'static TaskMetrics,
}

impl<F> Drop for Instrumented<F> {
    // glue: live-task gauge (N3 §5 "task 不泄漏")
    fn drop(&mut self) {
        self.metrics.live.dec();
    }
}

impl<F: Future> Future for Instrumented<F> {
    type Output = F::Output;

    // glue: N3 3.1 第 5 条 / 3.6 poll instrumentation
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        let start = Instant::now();
        let cfg = TASK.with(|t| {
            t.poll_start.set(start);
            t.calls.set(0);
            t.cfg
        });
        let before = if cfg.classify_stalls {
            crate::stall::sample()
        } else {
            None
        };
        let out = self.inner.as_mut().poll(cx);
        let end = Instant::now();
        let d = end.saturating_duration_since(start);
        TASK.with(|t| {
            t.cpu.set(t.cpu.get() + d);
            if let Some(r) = t.recorder.borrow().as_ref() {
                r.add(d);
            }
        });
        metrics::cpu_slice(d, end);
        if d > cfg.stall_threshold {
            let cause = match (before, crate::stall::sample()) {
                (Some(b), Some(a)) => crate::stall::classify(d, &b, &a),
                _ => crate::stall::StallCause::Unclassified,
            };
            STALLS.fetch_add(1, Ordering::Relaxed);
            crate::stall::count(cause);
            self.metrics.stall(d, cause);
            log::warn!(
                "poll stall ({}): task {} ran {:?} without yielding (threshold {:?})",
                cause.as_str(),
                self.metrics.name,
                d,
                cfg.stall_threshold
            );
        }
        out
    }
}

/// Wrap `fut` as a task: instrumented polls, in a fresh slice state. The CPU recorder of the
/// spawning task (if any) is inherited.
// glue: N3 3.1 task wrapper
pub(crate) fn instrument<F>(
    name: &'static str,
    cfg: SliceConfig,
    fut: F,
) -> impl Future<Output = F::Output> + Send + 'static
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let recorder = TASK
        .try_with(|t| t.recorder.borrow().clone())
        .ok()
        .flatten();
    let state = TaskState {
        cfg,
        poll_start: Cell::new(Instant::now()),
        calls: Cell::new(0),
        cpu: Cell::new(Duration::ZERO),
        recorder: RefCell::new(recorder),
    };
    let m = metrics::task(name);
    m.live.inc();
    TASK.scope(
        state,
        Instrumented {
            inner: Box::pin(fut),
            metrics: m,
        },
    )
}

/// The checks a checkpoint runs on every call: the statement's KILL signal and memory quota
/// (N3 3.3, A1 3.3). `ExecCtx` implements it with `SqlKiller` and the statement `MemTracker`,
/// which live in higher crates.
pub trait CheckpointHook: Sync {
    type Error;
    // glue: kill / memory check hook (implemented by ExecCtx)
    fn check(&self) -> Result<(), Self::Error>;
}

/// No checks: only the time slice.
impl CheckpointHook for () {
    type Error = std::convert::Infallible;

    // glue: no-op hook
    fn check(&self) -> Result<(), std::convert::Infallible> {
        Ok(())
    }
}

/// A checkpoint (mapping rule M-KILL): runs `hook` on every call, and yields the CPU when the
/// current poll has used up its time slice. `ExecCtx::checkpoint` delegates here.
///
/// Outside an instrumented task (one not started through the `ExecPool`) it only runs the
/// hook: there is no slice to measure.
// glue: N3 3.1 第 5 条 (TiKV RescheduleChecker); kill/memory checks are the caller's hook
pub async fn checkpoint<H: CheckpointHook + ?Sized>(hook: &H) -> Result<(), H::Error> {
    hook.check()?;
    if slice_used_up() {
        tokio::task::yield_now().await;
    }
    Ok(())
}

/// A checkpoint for per-row inner loops (hash build, sort, merge; mapping rule M-KILL): the
/// hook still runs on every call, the clock is read every `n` calls. Owned by the loop.
///
/// ```ignore
/// let mut cp = CheckpointStride::new(64);
/// for row in rows { cp.checkpoint(&ctx).await?; build(row); }
/// ```
pub struct CheckpointStride {
    every: u32,
    calls: u32,
}

impl CheckpointStride {
    // glue: N3 3.1 第 5 条 (per-row loops)
    pub fn new(every: u32) -> CheckpointStride {
        CheckpointStride {
            every: every.max(1),
            calls: 0,
        }
    }

    // glue: N3 3.1 第 5 条 (per-row loops)
    pub async fn checkpoint<H: CheckpointHook + ?Sized>(
        &mut self,
        hook: &H,
    ) -> Result<(), H::Error> {
        hook.check()?;
        self.calls += 1;
        if self.calls >= self.every {
            self.calls = 0;
            if poll_ran_longer_than_quantum() {
                tokio::task::yield_now().await;
            }
        }
        Ok(())
    }
}

// glue: N3 3.1 第 5 条
fn poll_ran_longer_than_quantum() -> bool {
    TASK.try_with(|t| t.poll_start.get().elapsed() >= t.cfg.quantum)
        .unwrap_or(false)
}

// glue: N3 3.1 第 5 条
fn slice_used_up() -> bool {
    TASK.try_with(|t| {
        let c = t.calls.get() + 1;
        if c < t.cfg.check_every {
            t.calls.set(c);
            return false;
        }
        t.calls.set(0);
        t.poll_start.get().elapsed() >= t.cfg.quantum
    })
    .unwrap_or(false)
}

/// Attach (or with `None` detach) a CPU recorder to the current task; tasks it spawns
/// afterwards inherit it. Returns the previous one. No-op outside an instrumented task.
// glue: CPU accounting per statement (N3 3.1 第 5 条)
pub fn set_cpu_recorder(r: Option<CpuRecorder>) -> Option<CpuRecorder> {
    TASK.try_with(|t| t.recorder.replace(r)).ok().flatten()
}

/// CPU (poll) time of the current task so far; `None` outside an instrumented task.
// glue: CPU accounting
pub fn task_cpu_time() -> Option<Duration> {
    TASK.try_with(|t| t.cpu.get()).ok()
}
