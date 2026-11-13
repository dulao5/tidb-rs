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
        let cfg = TASK.with(|t| t.cfg);
        // The CPU / switch samples bracket [start, end] (PR #10 review): a switch between the
        // two clock reads must fall inside the sampled interval, and nothing after `end` may.
        let before = if cfg.classify_stalls {
            crate::stall::sample()
        } else {
            None
        };
        let start = Instant::now();
        TASK.with(|t| {
            t.poll_start.set(start);
            t.calls.set(0);
        });
        let out = self.inner.as_mut().poll(cx);
        let end = Instant::now();
        let d = end.saturating_duration_since(start);
        let after = if before.is_some() && d > cfg.stall_threshold {
            crate::stall::sample()
        } else {
            None
        };
        match acct::mode() {
            acct::Mode::Off => {}
            acct::Mode::Instant => {
                TASK.with(|t| {
                    t.cpu.set(t.cpu.get() + d);
                    if let Some(r) = t.recorder.borrow().as_ref() {
                        r.add(d);
                    }
                });
                metrics::cpu_slice(d, end);
            }
            acct::Mode::Window(w) => {
                TASK.with(|t| {
                    t.cpu.set(t.cpu.get() + d);
                    acct::window_poll(t.recorder.borrow().as_ref(), d, end, w);
                });
                metrics::cpu_slice(d, end);
            }
        }
        if d > cfg.stall_threshold {
            let cause = match (before, after) {
                (Some(b), Some(a)) => crate::stall::classify(d, &b, &a),
                _ => crate::stall::StallCause::Unclassified,
            };
            STALLS.fetch_add(1, Ordering::Relaxed);
            crate::stall::count(cause);
            self.metrics.stall(d, cause);
            tracing::warn!(
                cause = cause.as_str(),
                task = self.metrics.name,
                elapsed = ?d,
                threshold = ?cfg.stall_threshold,
                "poll stall: a task ran without yielding"
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

/// Attach a fresh [`CpuRecorder`] to the current task with probability
/// `TIDB_RS_CPU_SAMPLE` (default 1), or detach it: the per-execution sampling decision of
/// N9 §6.1a, made at statement start. Benchmark switch, see [`acct`].
// glue: N9 §6.1a sampling (benchmark switch)
pub fn start_statement_sampling() {
    let sampled = acct::sample();
    set_cpu_recorder(sampled.then(CpuRecorder::new));
}

/// Benchmark switch for N9 §6.1 (not a design): the CPU accounting of [`Instrumented`],
/// chosen once per process by `TIDB_RS_CPU_ACCT`:
///
/// - `off`: no accounting (the poll's `Instant` reads stay: the time slice needs them);
/// - `instant` (default, the current behavior): each poll's wall time is added to the task and
///   its recorder;
/// - `win1` / `win10`: as `instant` for the task total; in addition every worker thread keeps a
///   window of the polls of sampled tasks (those with a recorder) and of the other polls, and
///   once 1 / 10 ms have passed reads `CLOCK_THREAD_CPUTIME_ID` once and splits the CPU since
///   the last read over the window's polls by wall time; the recorders get that CPU.
pub mod acct {
    use std::cell::RefCell;
    use std::sync::OnceLock;
    use std::time::Duration;
    use std::time::Instant;

    use super::CpuRecorder;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Mode {
        Off,
        Instant,
        Window(Duration),
    }

    static CONFIG: OnceLock<(Mode, f64)> = OnceLock::new();

    // glue: benchmark switch
    fn config() -> (Mode, f64) {
        *CONFIG.get_or_init(|| {
            let mode = match std::env::var("TIDB_RS_CPU_ACCT").as_deref() {
                Ok("off") => Mode::Off,
                Ok("win1") => Mode::Window(Duration::from_millis(1)),
                Ok("win10") => Mode::Window(Duration::from_millis(10)),
                _ => Mode::Instant,
            };
            let rate = std::env::var("TIDB_RS_CPU_SAMPLE")
                .ok()
                .and_then(|v| v.parse::<f64>().ok())
                .unwrap_or(1.0);
            tracing::info!(?mode, rate, "CPU accounting (N9 benchmark switch)");
            (mode, rate)
        })
    }

    // glue: benchmark switch
    pub fn mode() -> Mode {
        config().0
    }

    thread_local! {
        static RNG: std::cell::Cell<u64> = std::cell::Cell::new(
            0x9E37_79B9_7F4A_7C15 ^ (std::ptr::from_ref(&()) as usize as u64),
        );
        static WIN: RefCell<Option<Win>> = const { RefCell::new(None) };
    }

    /// One random draw against the sample rate (xorshift; quality is not a concern here).
    // glue: benchmark switch
    pub(super) fn sample() -> bool {
        let rate = config().1;
        if rate >= 1.0 {
            return true;
        }
        if rate <= 0.0 {
            return false;
        }
        RNG.with(|r| {
            let mut x = r.get() | 1;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            r.set(x);
            ((x >> 11) as f64 / (1u64 << 53) as f64) < rate
        })
    }

    struct Win {
        last_cpu: Duration,
        last_read: Instant,
        polls: Vec<(CpuRecorder, u64)>,
        sampled_wall: u64,
        other_wall: u64,
    }

    // glue: benchmark switch
    fn thread_cpu() -> Duration {
        nix::time::clock_gettime(nix::time::ClockId::CLOCK_THREAD_CPUTIME_ID).map_or(
            Duration::ZERO,
            |ts| {
                Duration::new(
                    u64::try_from(ts.tv_sec()).unwrap_or(0),
                    u32::try_from(ts.tv_nsec()).unwrap_or(0),
                )
            },
        )
    }

    /// Record one poll of `wall` ending at `end` in this thread's window.
    // glue: benchmark switch
    pub(super) fn window_poll(
        rec: Option<&CpuRecorder>,
        wall: Duration,
        end: Instant,
        win: Duration,
    ) {
        let wall = u64::try_from(wall.as_nanos()).unwrap_or(u64::MAX);
        WIN.with(|w| {
            let mut w = w.borrow_mut();
            let Some(rec) = rec else {
                if let Some(w) = w.as_mut() {
                    w.other_wall += wall;
                }
                return;
            };
            let w = w.get_or_insert_with(|| Win {
                last_cpu: thread_cpu(),
                last_read: end,
                polls: Vec::with_capacity(64),
                sampled_wall: 0,
                other_wall: 0,
            });
            match w.polls.last_mut() {
                Some((r, n)) if std::sync::Arc::ptr_eq(&r.0, &rec.0) => *n += wall,
                _ => w.polls.push((rec.clone(), wall)),
            }
            w.sampled_wall += wall;
            if end.saturating_duration_since(w.last_read) >= win {
                let cpu = thread_cpu();
                let delta = cpu.saturating_sub(w.last_cpu).as_nanos();
                let total = u128::from(w.sampled_wall + w.other_wall);
                for (r, n) in w.polls.drain(..) {
                    if total > 0 {
                        let share = delta * u128::from(n) / total;
                        r.add(Duration::from_nanos(
                            u64::try_from(share).unwrap_or(u64::MAX),
                        ));
                    }
                }
                w.sampled_wall = 0;
                w.other_wall = 0;
                w.last_cpu = cpu;
                w.last_read = end;
            }
        });
    }
}
