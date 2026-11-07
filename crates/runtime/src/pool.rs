//! The one runtime of the process and the [`ExecPool`] handle (N3 3.1 第 2、3 条, 3.2).
//!
//! - **One runtime, no hand-off.** Network, statement execution and the KV client all run on
//!   it. A connection task is also its statement task; a new task is spawned only where Go
//!   starts a goroutine (mapping rule M-GO).
//! - **Two topologies, same code** (D32). [`Topology::Steal`] is a tokio multi-thread
//!   (work-stealing) runtime, the default. [`Topology::NoSteal`] is N tokio current-thread
//!   runtimes, each on its own OS thread: a connection is placed on one of them
//!   ([`ExecPool::spawn_connection`], round robin) and the tasks it spawns stay on that thread
//!   (pingora-runtime's NoSteal, without the dependency). All code must be `Send` and must not
//!   rely on thread-local state, so that both topologies stay possible; the test suite runs on
//!   both.
//! - **`spawn_with_recover`** is Go `util.WithRecovery(exec, recoverFn)` for a task:
//!   `on_exit` runs whether or not the task panicked, and a panic is turned into a
//!   [`PanicError`] at the task boundary (mapping rule M-PANIC); it never crosses into other
//!   tasks.
//!
//! Every task is instrumented ([`crate::slice`]): time-sliced checkpoints, CPU accounting,
//! poll-stall detection, and a live-task gauge by name.

use std::any::Any;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::task::Context;
use std::task::Poll;

use tokio::runtime::Handle;
use tokio::task::JoinHandle;

use crate::metrics;
use crate::slice;
use crate::slice::SliceConfig;

/// Thread topology of the runtime (N3 3.1 第 2 条).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Topology {
    /// tokio multi-thread runtime with work stealing. Default.
    #[default]
    Steal,
    /// One tokio current-thread runtime per worker thread; a connection and the tasks it
    /// spawns stay on one thread.
    NoSteal,
}

/// Runtime configuration.
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    pub topology: Topology,
    /// Worker threads. Default: the number of CPUs (Go GOMAXPROCS).
    pub worker_threads: usize,
    /// Thread name prefix.
    pub thread_name: String,
    pub slice: SliceConfig,
}

impl Default for RuntimeConfig {
    // glue: N3 defaults
    fn default() -> RuntimeConfig {
        RuntimeConfig {
            topology: Topology::Steal,
            worker_threads: std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1),
            thread_name: "tidb-rs-exec".to_owned(),
            slice: SliceConfig::default(),
        }
    }
}

/// A recovered panic of a task (Go: the value passed to `recoverFn`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanicError {
    /// The panic message (Go formats the recovered value with `%v`).
    pub message: String,
}

impl PanicError {
    // glue: panic payload → Go's `%v` of the recovered value
    fn from_payload(p: &(dyn Any + Send)) -> PanicError {
        let message = if let Some(s) = p.downcast_ref::<&'static str>() {
            (*s).to_owned()
        } else if let Some(s) = p.downcast_ref::<String>() {
            s.clone()
        } else if let Some(e) = p.downcast_ref::<tidb_base::Error>() {
            // Go `panic(err)` recovered and formatted with %v: the error text.
            e.to_string()
        } else {
            "<non-string panic payload>".to_owned()
        };
        PanicError { message }
    }
}

impl std::fmt::Display for PanicError {
    // glue: Display
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for PanicError {}

/// How a [`ExecPool::spawn_with_recover`] task ended (Go `recoverFn(r)`: `r == nil` is
/// `Completed`, a panic value is `Panicked`). `Cancelled` has no Go counterpart: a goroutine
/// cannot be stopped from outside, a task can (`abort`, runtime shutdown, the future dropped).
/// `on_exit` still runs then, so callers that `Done()` a wait group or close a channel in it
/// never leave a waiter hanging.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskExit {
    Completed,
    Panicked(PanicError),
    Cancelled,
}

/// Calls `on_exit` exactly once: with the outcome, or with `Cancelled` when dropped first.
struct ExitGuard<E: FnOnce(TaskExit)>(Option<E>);

impl<E: FnOnce(TaskExit)> ExitGuard<E> {
    // glue: N3 3.2 exit hook
    fn fire(mut self, e: TaskExit) {
        if let Some(f) = self.0.take() {
            f(e);
        }
    }
}

impl<E: FnOnce(TaskExit)> Drop for ExitGuard<E> {
    // glue: N3 3.2 exit hook on cancellation
    fn drop(&mut self) {
        if let Some(f) = self.0.take() {
            f(TaskExit::Cancelled);
        }
    }
}

/// Pool ids, to recognise a NoSteal worker thread of a given pool.
static NEXT_POOL_ID: AtomicUsize = AtomicUsize::new(1);

thread_local! {
    /// On a NoSteal worker thread: (pool id, runtime index). Runtime infrastructure only;
    /// executor code must not use thread-local state (N3 3.1 第 2 条).
    static NO_STEAL_THREAD: std::cell::Cell<(usize, usize)> = const { std::cell::Cell::new((0, 0)) };
}

struct Inner {
    id: usize,
    handles: Vec<Handle>,
    next: AtomicUsize,
    slice: SliceConfig,
    topology: Topology,
}

/// Owns the runtime threads. Dropping it shuts the runtime down.
pub struct Runtime {
    pool: ExecPool,
    steal: Option<tokio::runtime::Runtime>,
    no_steal: Vec<(
        tokio::sync::oneshot::Sender<()>,
        std::thread::JoinHandle<()>,
    )>,
}

impl Runtime {
    // glue: N3 3.1 runtime construction
    pub fn new(cfg: RuntimeConfig) -> std::io::Result<Runtime> {
        let n = cfg.worker_threads.max(1);
        match cfg.topology {
            Topology::Steal => {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(n)
                    .thread_name(cfg.thread_name.clone())
                    .enable_all()
                    .build()?;
                let id = NEXT_POOL_ID.fetch_add(1, Ordering::Relaxed);
                let pool = ExecPool::new(id, vec![rt.handle().clone()], cfg.slice, cfg.topology);
                Ok(Runtime {
                    pool,
                    steal: Some(rt),
                    no_steal: Vec::new(),
                })
            }
            Topology::NoSteal => {
                let id = NEXT_POOL_ID.fetch_add(1, Ordering::Relaxed);
                let mut handles = Vec::with_capacity(n);
                let mut threads = Vec::with_capacity(n);
                for i in 0..n {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()?;
                    handles.push(rt.handle().clone());
                    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
                    let t = std::thread::Builder::new()
                        .name(format!("{}-{i}", cfg.thread_name))
                        .spawn(move || {
                            NO_STEAL_THREAD.with(|c| c.set((id, i)));
                            // Drives this thread's runtime (its tasks, timers and I/O) until
                            // shutdown.
                            rt.block_on(async {
                                let _ = rx.await;
                            });
                        })?;
                    threads.push((tx, t));
                }
                let pool = ExecPool::new(id, handles, cfg.slice, cfg.topology);
                Ok(Runtime {
                    pool,
                    steal: None,
                    no_steal: threads,
                })
            }
        }
    }

    // glue: accessor
    pub fn pool(&self) -> &ExecPool {
        &self.pool
    }

    /// Run `fut` as a connection-level task and wait for it from outside the runtime (the
    /// process `main`, tests).
    // glue: entry point
    pub fn block_on<F>(&self, fut: F) -> F::Output
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let h = self.pool.spawn_connection("block_on", fut);
        let handle = self.pool.pick();
        match handle.block_on(h) {
            Ok(v) => v,
            Err(e) => std::panic::resume_unwind(e.into_panic()),
        }
    }
}

impl Drop for Runtime {
    // glue: shutdown
    fn drop(&mut self) {
        if let Some(rt) = self.steal.take() {
            // Waits for the worker threads, which drop the remaining tasks (so their
            // `spawn_with_recover` exit hooks run); blocking tasks get a bounded wait.
            rt.shutdown_timeout(std::time::Duration::from_secs(5));
        }
        for (tx, t) in self.no_steal.drain(..) {
            let _ = tx.send(());
            let _ = t.join();
        }
    }
}

/// Handle to spawn tasks on the runtime (`ExecPool` of foundation-interfaces). Cheap to
/// clone; `Send + Sync`.
#[derive(Clone)]
pub struct ExecPool {
    inner: Arc<Inner>,
}

impl ExecPool {
    // glue: constructor
    fn new(id: usize, handles: Vec<Handle>, slice: SliceConfig, topology: Topology) -> ExecPool {
        ExecPool {
            inner: Arc::new(Inner {
                id,
                handles,
                next: AtomicUsize::new(0),
                slice,
                topology,
            }),
        }
    }

    // glue: accessor
    pub fn topology(&self) -> Topology {
        self.inner.topology
    }

    // glue: accessor
    pub fn slice_config(&self) -> SliceConfig {
        self.inner.slice
    }

    /// Round robin over the runtimes (one for Steal).
    // glue: placement
    fn pick(&self) -> &Handle {
        let h = &self.inner.handles;
        let i = self.inner.next.fetch_add(1, Ordering::Relaxed) % h.len();
        &h[i]
    }

    /// The runtime of the calling task if it is one of ours (NoSteal: stay on this thread),
    /// else a round-robin pick.
    // glue: placement
    fn local(&self) -> &Handle {
        let h = &self.inner.handles;
        if h.len() == 1 {
            return &h[0];
        }
        let (id, i) = NO_STEAL_THREAD.with(|c| c.get());
        if id == self.inner.id {
            return &h[i];
        }
        self.pick()
    }

    /// Place a new connection (N3 3.1 第 2 条): round robin over the runtimes. Its statements
    /// run in this task; tasks it spawns stay on its runtime.
    // glue: N3 3.1 connection placement
    pub fn spawn_connection<F>(&self, name: &'static str, fut: F) -> JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.pick()
            .spawn(slice::instrument(name, self.inner.slice, fut))
    }

    /// `go f()` (mapping rule M-GO): a task on the caller's runtime.
    // glue: N3 3.1 第 3 条 (M-GO)
    pub fn spawn<F>(&self, name: &'static str, fut: F) -> JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.local()
            .spawn(slice::instrument(name, self.inner.slice, fut))
    }

    /// Go `util.WithRecovery(exec, recoverFn)` as a task (N3 3.2, mapping rule M-PANIC):
    /// `on_exit` is called exactly once when the task ends, however it ends: completed,
    /// panicked (the panic is logged and does not propagate) or cancelled (`abort`, runtime
    /// shutdown, also before it first ran). The handle yields `None` if the task panicked.
    ///
    /// `on_exit` runs inside the task, before anyone awaiting the handle wakes up, which is
    /// the order Go's deferred `recoverFn` gives.
    // go: pkg/util/misc.go WithRecovery L2 (a task: the hook also runs on cancellation, which goroutines do not have; panics are caught per poll)
    pub fn spawn_with_recover<F, R>(
        &self,
        name: &'static str,
        fut: F,
        on_exit: impl FnOnce(TaskExit) + Send + 'static,
    ) -> JoinHandle<Option<R>>
    where
        F: Future<Output = R> + Send + 'static,
        R: Send + 'static,
    {
        // Created before the task first runs, so dropping the unstarted task still fires it.
        let guard = ExitGuard(Some(on_exit));
        let task = async move {
            let guard = guard;
            match CatchUnwind(Box::pin(fut)).await {
                Ok(v) => {
                    guard.fire(TaskExit::Completed);
                    Some(v)
                }
                Err(payload) => {
                    let e = PanicError::from_payload(payload.as_ref());
                    metrics::panic();
                    // Go: logutil.BgLogger().Error("panic in the recoverable goroutine", ...),
                    // after recoverFn.
                    let msg = e.to_string();
                    guard.fire(TaskExit::Panicked(e));
                    log::error!("panic in the recoverable goroutine: task {name}: {msg}");
                    None
                }
            }
        };
        self.spawn(name, task)
    }
}

/// `catch_unwind` around each poll of a future.
struct CatchUnwind<F>(Pin<Box<F>>);

impl<F: Future> Future for CatchUnwind<F> {
    type Output = Result<F::Output, Box<dyn Any + Send>>;

    // glue: task-boundary catch_unwind (M-PANIC)
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let inner = self.0.as_mut();
        match std::panic::catch_unwind(AssertUnwindSafe(|| inner.poll(cx))) {
            Ok(Poll::Pending) => Poll::Pending,
            Ok(Poll::Ready(v)) => Poll::Ready(Ok(v)),
            Err(p) => Poll::Ready(Err(p)),
        }
    }
}
