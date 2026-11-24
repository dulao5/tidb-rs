//! go-file: pkg/executor/projection.go
//!
//! The physical Projection operator (the basic executors, D12.2): un-parallel, or parallel
//! with one input fetcher and `tidb_projection_concurrency` workers.
//!
//! The goroutines are tokio tasks and the Go channels tokio channels of the same capacities
//! (mapping rules M-GO, M-CH):
//! - `finishCh` (closed by `Close`) is a `watch` channel set to `true`;
//! - the fetcher, the workers and `Next` pass `projectionInput` by value and share each
//!   `projectionOutput` (Go passes pointers): its chunk is behind a mutex that nobody holds
//!   across an `.await` (M-LOCK), its `done` channel has capacity 1 as in Go;
//! - `wg` is the tasks' join handles, awaited by `Close` (M-WG); each task gives back what Go
//!   leaves in the executor's fields after the goroutine exits (its channels, and for the
//!   fetcher the child executor it ran), so that `Close` drains the channels as Go does and
//!   the base executor closes the child;
//! - the deferred `recover()` of each goroutine is a `catch_unwind` around the task's loop
//!   (M-PANIC); an error of a memory tracker's action (Go: a panic) takes the same path.

use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use async_trait::async_trait;
use futures::FutureExt;
use tidb_base::Error;
use tidb_chunk::chunk::Chunk;
use tidb_execdetails::execdetails::{
    ConcurrencyInfo, RuntimeStatsColl, RuntimeStatsWithConcurrencyInfo,
};
use tidb_expression::eval::RpnState;
use tidb_expression::evaluator::EvaluatorSuite;
use tidb_expression::expression::EvalState;
use tidb_memory::tracker::{Tracker, new_tracker};
use tidb_sessionctx::exprctx::EvalContextRef;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use crate::exec::ExecCtx;
use crate::glue::SessionContext;
use crate::internal::exec::executor::{self as exec, BaseExecutorV2, Executor};
use crate::select::consume;

// This file contains the implementation of the physical Projection Operator:
// https://en.wikipedia.org/wiki/Projection_(relational_algebra)
//
// NOTE:
// 1. The number of "projectionWorker" is controlled by the global session
//    variable "tidb_projection_concurrency".
// 2. Unparallel version is used when one of the following situations occurs:
//    a. "tidb_projection_concurrency" is set to 0.
//    b. The estimated input size is smaller than "tidb_max_chunk_size".
//    c. This projection can not be executed vectorially.

/// Go `projectionInput`; `targetWorker` is the worker's index in `ProjectionExec.workers`.
pub(crate) struct ProjectionInput {
    chk: Chunk,
    target_worker: usize,
}

/// Go `projectionOutput`, shared by the fetcher, a worker and `Next` (Go: a pointer).
pub(crate) struct ProjectionOutput {
    chk: parking_lot::Mutex<Chunk>,
    /// `done chan error` (capacity 1): both ends, the output being shared.
    done_tx: mpsc::Sender<Result<(), Error>>,
    done_rx: tokio::sync::Mutex<mpsc::Receiver<Result<(), Error>>>,
}

type OutputRef = Arc<ProjectionOutput>;

impl ProjectionOutput {
    /// `&projectionOutput{chk: chk, done: make(chan error, 1)}`.
    // glue: constructor (Go composite literal)
    fn new(chk: Chunk) -> OutputRef {
        let (done_tx, done_rx) = mpsc::channel(1);
        Arc::new(ProjectionOutput {
            chk: parking_lot::Mutex::new(chk),
            done_tx,
            done_rx: tokio::sync::Mutex::new(done_rx),
        })
    }
}

/// projectionExecutorContext is the execution context for the `ProjectionExec`
#[derive(Clone)]
pub struct ProjectionExecutorContext {
    /// `nil` (no statement tracker: contexts that did not run `ResetContextOfStmt`) is `None`.
    pub(crate) stmt_mem_tracker: Option<Arc<Tracker>>,
    pub(crate) stmt_runtime_stats_coll: Option<Arc<RuntimeStatsColl>>,
    pub(crate) eval_ctx: EvalContextRef,
    pub(crate) enable_vectorized_expression: bool,
}

// go: newProjectionExecutorContext L1 [M-CTX]
pub fn new_projection_executor_context(sctx: &dyn SessionContext) -> ProjectionExecutorContext {
    ProjectionExecutorContext {
        stmt_mem_tracker: sctx.get_session_vars().stmt_ctx().mem_tracker.clone(),
        stmt_runtime_stats_coll: sctx
            .get_session_vars()
            .stmt_ctx()
            .runtime_stats_coll
            .clone(),
        eval_ctx: sctx.get_expr_ctx().get_eval_ctx(),
        enable_vectorized_expression: sctx.get_session_vars().enable_vectorized_expression,
    }
}

/// ProjectionExec implements the physical Projection Operator:
/// https://en.wikipedia.org/wiki/Projection_(relational_algebra)
///
/// The evaluation state of the un-parallel execution (`eval_state`, `rpn`) belongs to the
/// executor; each worker has its own (N4 §3.4: no correlated values, Apply is not ported).
pub struct ProjectionExec {
    pub(crate) ctx: ProjectionExecutorContext,
    pub(crate) base: BaseExecutorV2,

    pub(crate) evaluator_suit: Arc<EvaluatorSuite>,

    /// `finishCh`: `true` is Go's `close(finishCh)`.
    finish_tx: Option<watch::Sender<bool>>,
    /// `outputCh`, the receiving end (`Next`); the fetcher holds the sending end.
    output_rx: Option<mpsc::Receiver<OutputRef>>,
    /// `fetcher.outputCh`, the sending end (`Next` gives outputs back to the fetcher).
    fetcher_output_tx: Option<mpsc::Sender<OutputRef>>,
    pub(crate) num_workers: i64,
    /// The fetcher's and the workers' tasks (Go: `fetcher`, `workers` and `wg`).
    fetcher_handle: Option<JoinHandle<ProjectionInputFetcher>>,
    worker_handles: Vec<JoinHandle<ProjectionWorker>>,
    pub(crate) child_result: Option<Chunk>,

    /// parentReqRows indicates how many rows the parent executor is
    /// requiring. It is set when parallelExecute() is called and used by the
    /// concurrent projectionInputFetcher.
    ///
    /// NOTE: It should be protected by atomic operations.
    parent_req_rows: Arc<AtomicI64>,

    pub(crate) mem_tracker: Option<Arc<Tracker>>,

    pub(crate) calculate_no_delay: bool,
    pub(crate) prepared: bool,

    pub(crate) eval_state: EvalState,
    pub(crate) rpn: RpnState,
}

impl ProjectionExec {
    /// The executor as the builder makes it (Go: the composite literal of `buildProjection`).
    // glue: constructor (Go composite literal)
    pub fn new(
        ctx: ProjectionExecutorContext,
        base: BaseExecutorV2,
        num_workers: i64,
        evaluator_suit: EvaluatorSuite,
        calculate_no_delay: bool,
    ) -> ProjectionExec {
        ProjectionExec {
            ctx,
            base,
            evaluator_suit: Arc::new(evaluator_suit),
            finish_tx: None,
            output_rx: None,
            fetcher_output_tx: None,
            num_workers,
            fetcher_handle: None,
            worker_handles: Vec::new(),
            child_result: None,
            parent_req_rows: Arc::new(AtomicI64::new(0)),
            mem_tracker: None,
            calculate_no_delay,
            prepared: false,
            eval_state: EvalState::new(),
            rpn: RpnState::default(),
        }
    }
}

#[async_trait]
impl Executor for ProjectionExec {
    // glue: M-SELF
    fn base(&self) -> &BaseExecutorV2 {
        &self.base
    }
    // glue: M-SELF
    fn base_mut(&mut self) -> &mut BaseExecutorV2 {
        &mut self.base
    }
    // glue: M-IFACE (type assertion to *ProjectionExec)
    fn as_projection(&self) -> Option<&ProjectionExec> {
        Some(self)
    }

    /// Open implements the Executor Open interface.
    // go: ProjectionExec.Open L2 (no failpoint)
    async fn open(&mut self, ctx: &ExecCtx) -> Result<(), Error> {
        self.base.open(ctx).await?;
        self.open_inner(ctx)
    }

    /// Next implements the Executor Next interface.
    ///
    /// Here we explain the execution flow of the parallel projection implementation.
    /// There are 3 main components:
    ///   1. "projectionInputFetcher": Fetch input "Chunk" from child.
    ///   2. "projectionWorker":       Do the projection work.
    ///   3. "ProjectionExec.Next":    Return result to parent.
    ///
    /// 1. "projectionInputFetcher" gets its input and output resources from its
    ///    "inputCh" and "outputCh" channel, once the input and output resources are
    ///    obtained, it fetches child's result into "input.chk" and:
    ///    a. Dispatches this input to the worker specified in "input.targetWorker"
    ///    b. Dispatches this output to the main thread: "ProjectionExec.Next"
    ///    c. Dispatches this output to the worker specified in "input.targetWorker"
    ///
    ///    It is finished and exited once:
    ///    a. There is no more input from child.
    ///    b. "ProjectionExec" close the "globalFinishCh"
    ///
    /// 2. "projectionWorker" gets its input and output resources from its
    ///    "inputCh" and "outputCh" channel, once the input and output resources are
    ///    abtained, it calculates the projection result use "input.chk" as the input
    ///    and "output.chk" as the output, once the calculation is done, it:
    ///    a. Sends "nil" or error to "output.done" to mark this input is finished.
    ///    b. Returns the "input" resource to "projectionInputFetcher.inputCh"
    ///
    ///    They are finished and exited once:
    ///    a. "ProjectionExec" closes the "globalFinishCh"
    ///
    /// 3. "ProjectionExec.Next" gets its output resources from its "outputCh" channel.
    ///    After receiving an output from "outputCh", it should wait to receive a "nil"
    ///    or error from "output.done" channel. Once a "nil" or error is received:
    ///    a. Returns this output to its parent
    ///    b. Returns the "output" resource to "projectionInputFetcher.outputCh"
    // go: ProjectionExec.Next L1
    async fn next(&mut self, ctx: &ExecCtx, req: &mut Chunk) -> Result<(), Error> {
        req.grow_and_reset(self.max_chunk_size().max(0) as usize);
        if self.is_unparallel_exec() {
            return self.un_parallel_execute(ctx, req).await;
        }
        self.parallel_execute(ctx, req).await
    }

    /// Close implements the Executor Close interface.
    // go: ProjectionExec.Close L2 (the tasks give back their channels and the child: M-WG)
    async fn close(&mut self) -> Result<(), Error> {
        // if e.BaseExecutor.Open returns error, e.childResult will be nil, see `24210`
        // for more information
        if self.is_unparallel_exec()
            && let Some(child_result) = self.child_result.take()
        {
            // Releasing memory never runs an action.
            let _ = consume(&self.mem_tracker, -child_result.memory_usage());
        }
        if self.prepared {
            if let Some(finish) = &self.finish_tx {
                finish.send_replace(true);
            }
            // Wait for fetcher and workers to finish and exit. Each handle leaves `self`
            // only once its task has ended, so a close dropped halfway still leaves the
            // running tasks to `Drop`, which aborts them.
            let fetcher = match self.fetcher_handle.as_mut() {
                Some(h) => {
                    let r = h.await;
                    self.fetcher_handle = None;
                    joined(r, "fetcher")
                }
                None => None,
            };
            let mut workers = Vec::with_capacity(self.worker_handles.len());
            while let Some(h) = self.worker_handles.first_mut() {
                let r = h.await;
                self.worker_handles.remove(0);
                workers.push(joined(r, "worker"));
            }

            // clear fetcher
            if let Some(fetcher) = fetcher {
                self.base.meta.set_all_children(vec![fetcher.child]);
                self.drain_input_ch(fetcher.input_rx);
                self.drain_output_ch(fetcher.output_rx);
            }

            // clear workers
            for w in workers.into_iter().flatten() {
                self.drain_input_ch(w.input_rx);
                self.drain_output_ch(w.output_rx);
            }
        }
        if self.base.stats.runtime_stats().is_some() {
            let runtime_stats = RuntimeStatsWithConcurrencyInfo::default();
            if self.is_unparallel_exec() {
                runtime_stats.set_concurrency_info(&[ConcurrencyInfo::new("Concurrency", 0)]);
            } else {
                runtime_stats
                    .set_concurrency_info(&[ConcurrencyInfo::new("Concurrency", self.num_workers)]);
            }
            if let Some(coll) = &self.ctx.stmt_runtime_stats_coll {
                coll.register_stats(self.base.meta.id() as i64, Box::new(runtime_stats));
            }
        }
        self.base.close().await
    }
}

impl ProjectionExec {
    // go: ProjectionExec.open L1
    fn open_inner(&mut self, _ctx: &ExecCtx) -> Result<(), Error> {
        self.prepared = false;
        self.parent_req_rows
            .store(self.max_chunk_size() as i64, Ordering::SeqCst);

        let mem_tracker = match &self.mem_tracker {
            Some(t) => {
                t.reset()?;
                Arc::clone(t)
            }
            None => {
                let t = new_tracker(self.base.meta.id(), -1);
                self.mem_tracker = Some(Arc::clone(&t));
                t
            }
        };
        if let Some(parent) = &self.ctx.stmt_mem_tracker {
            mem_tracker.attach_to(parent)?;
        }

        // For now a Projection can not be executed vectorially only because it
        // contains "SetVar" or "GetVar" functions, in this scenario this
        // Projection can not be executed parallelly.
        if self.num_workers > 0 && !self.evaluator_suit.vectorizable() {
            self.num_workers = 0;
        }

        if self.is_unparallel_exec() {
            let child_result = exec::try_new_cache_chunk(self.base.meta.children(0).as_ref());
            mem_tracker.consume(child_result.memory_usage())?;
            self.child_result = Some(child_result);
        }

        // e.wg = &sync.WaitGroup{}: no task of an earlier run is left (Close joined them).
        self.fetcher_handle = None;
        self.worker_handles = Vec::new();

        Ok(())
    }

    // go: ProjectionExec.isUnparallelExec L1
    fn is_unparallel_exec(&self) -> bool {
        self.num_workers <= 0
    }

    // go: ProjectionExec.unParallelExecute L2 (no failpoint)
    async fn un_parallel_execute(&mut self, ctx: &ExecCtx, chk: &mut Chunk) -> Result<(), Error> {
        let max_chunk_size = self.max_chunk_size().max(0) as usize;
        let child_result = self
            .child_result
            .as_mut()
            .expect("ProjectionExec.Next before Open or after Close (Go: nil chunk)");
        // transmit the requiredRows
        child_result.set_required_rows(chk.required_rows() as isize, max_chunk_size);
        let m_size = child_result.memory_usage();
        let err = exec::next(ctx, self.base.meta.children(0).as_mut(), child_result).await;
        consume(&self.mem_tracker, child_result.memory_usage() - m_size)?;
        err?;
        if child_result.num_rows() == 0 {
            return Ok(());
        }
        self.evaluator_suit.run(
            &*self.ctx.eval_ctx,
            &self.eval_state,
            &mut self.rpn,
            self.ctx.enable_vectorized_expression,
            child_result,
            chk,
        )
    }

    // go: ProjectionExec.parallelExecute L2 (no failpoint; M-CH, M-LOCK)
    async fn parallel_execute(&mut self, ctx: &ExecCtx, chk: &mut Chunk) -> Result<(), Error> {
        self.parent_req_rows
            .store(chk.required_rows() as i64, Ordering::SeqCst);
        if !self.prepared {
            self.prepare(ctx)?;
            self.prepared = true;
        }

        let output_rx = self
            .output_rx
            .as_mut()
            .expect("prepared ProjectionExec without its output channel");
        let Some(output) = output_rx.recv().await else {
            return Ok(());
        };

        // A `None` (every sender gone) cannot happen: the output holds its own sender.
        if let Some(Err(err)) = output.done_rx.lock().await.recv().await {
            return Err(err);
        }
        {
            let mut output_chk = output.chk.lock();
            let m_size = output_chk.memory_usage();
            chk.swap_columns(&mut output_chk);
            consume(&self.mem_tracker, output_chk.memory_usage() - m_size)?;
        }
        if let Some(tx) = &self.fetcher_output_tx {
            // The fetcher's receiver lives until Close joins the fetcher.
            let _ = tx.send(output).await;
        }
        Ok(())
    }

    /// Go starts the goroutines with `go`; a tracker's action (Go: a panic out of `Next`)
    /// is an error, returned before any task starts.
    // go: ProjectionExec.prepare L2 (no failpoint; tracker actions are errors, M-PANIC; M-GO, M-CH)
    fn prepare(&mut self, ctx: &ExecCtx) -> Result<(), Error> {
        let num_workers = self.num_workers as usize;
        let mem_tracker = Arc::clone(
            self.mem_tracker
                .as_ref()
                .expect("ProjectionExec.prepare before Open"),
        );
        let (finish_tx, finish_rx) = watch::channel(false);
        let (output_tx, output_rx) = mpsc::channel(num_workers);

        // Initialize projectionInputFetcher.
        let (fetcher_input_tx, fetcher_input_rx) = mpsc::channel(num_workers);
        let (fetcher_output_tx, fetcher_output_rx) = mpsc::channel(num_workers);

        // Initialize projectionWorker.
        let mut workers = Vec::with_capacity(num_workers);
        let mut worker_chs = Vec::with_capacity(num_workers);
        for i in 0..num_workers {
            let (input_tx, input_rx) = mpsc::channel(1);
            let (worker_output_tx, worker_output_rx) = mpsc::channel(1);
            workers.push(ProjectionWorker {
                ctx: self.ctx.clone(),
                evaluator_suit: Arc::clone(&self.evaluator_suit),
                global_finish_rx: finish_rx.clone(),
                input_give_back_tx: fetcher_input_tx.clone(),
                input_rx,
                output_rx: worker_output_rx,
                mem_tracker: Arc::clone(&mem_tracker),
                eval_state: EvalState::new(),
                rpn: RpnState::default(),
            });
            worker_chs.push((input_tx, worker_output_tx));

            let input_chk = exec::new_first_chunk(self.base.meta.children(0).as_ref());
            mem_tracker.consume(input_chk.memory_usage())?;
            // Capacity numWorkers, numWorkers inputs: never full.
            let _ = fetcher_input_tx.try_send(ProjectionInput {
                chk: input_chk,
                target_worker: i,
            });

            let output_chk = exec::new_first_chunk(&*self);
            mem_tracker.consume(output_chk.memory_usage())?;
            let _ = fetcher_output_tx.try_send(ProjectionOutput::new(output_chk));
        }

        let child = self
            .base
            .meta
            .take_all_children()
            .into_iter()
            .next()
            .expect("ProjectionExec without a child");
        let fetcher = ProjectionInputFetcher {
            child,
            global_finish_rx: finish_rx,
            global_output_tx: Some(output_tx),
            input_rx: fetcher_input_rx,
            output_rx: fetcher_output_rx,
            workers: worker_chs,
            mem_tracker,
            parent_req_rows: Arc::clone(&self.parent_req_rows),
            max_chunk_size: self.max_chunk_size().max(0) as usize,
        };
        // The fetcher's own sender of its input channel is the workers' (inputGiveBackCh).
        drop(fetcher_input_tx);

        self.finish_tx = Some(finish_tx);
        self.output_rx = Some(output_rx);
        self.fetcher_output_tx = Some(fetcher_output_tx);

        self.fetcher_handle = Some(spawn(ctx, "ProjectionFetcher", fetcher.run(ctx.clone())));

        for w in workers {
            self.worker_handles
                .push(spawn(ctx, "ProjectionWorker", w.run(ctx.clone())));
        }
        Ok(())
    }

    // go: ProjectionExec.drainInputCh L1 [M-CH]
    fn drain_input_ch(&self, mut ch: mpsc::Receiver<ProjectionInput>) {
        ch.close();
        while let Ok(item) = ch.try_recv() {
            // Releasing memory never runs an action.
            let _ = consume(&self.mem_tracker, -item.chk.memory_usage());
        }
    }

    // go: ProjectionExec.drainOutputCh L1 [M-CH]
    fn drain_output_ch(&self, mut ch: mpsc::Receiver<OutputRef>) {
        ch.close();
        while let Ok(item) = ch.try_recv() {
            let usage = item.chk.lock().memory_usage();
            let _ = consume(&self.mem_tracker, -usage);
        }
    }
}

/// A statement dropped without `Close` (a cancelled future, M-DEFER): the tasks are told to
/// finish and stopped, so none outlives the executor. Go has no such path (a goroutine is
/// never dropped).
impl Drop for ProjectionExec {
    // glue: cancellation of the tasks (M-DEFER)
    fn drop(&mut self) {
        if let Some(finish) = &self.finish_tx {
            finish.send_replace(true);
        }
        if let Some(h) = &self.fetcher_handle {
            h.abort();
        }
        for h in &self.worker_handles {
            h.abort();
        }
    }
}

/// `go f()` (M-GO): on the statement's `ExecPool` when the context has one, else on the
/// current tokio runtime (the one the statement runs on).
// glue: go f() on the N3 ExecPool
fn spawn<F>(ctx: &ExecCtx, name: &'static str, fut: F) -> JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    match &ctx.pool {
        Some(pool) => pool.spawn(name, fut),
        None => tokio::spawn(fut),
    }
}

/// Go `projectionInputFetcher`. Its `proj` (the executor) gives it the memory tracker, the
/// parent's required rows and the max chunk size; `targetWorker.inputCh` / `.outputCh` are
/// the senders in `workers`.
pub(crate) struct ProjectionInputFetcher {
    child: Box<dyn Executor>,
    global_finish_rx: watch::Receiver<bool>,
    /// `globalOutputCh`: dropping it is Go's `close(f.globalOutputCh)`.
    global_output_tx: Option<mpsc::Sender<OutputRef>>,

    input_rx: mpsc::Receiver<ProjectionInput>,
    output_rx: mpsc::Receiver<OutputRef>,

    workers: Vec<(mpsc::Sender<ProjectionInput>, mpsc::Sender<OutputRef>)>,
    mem_tracker: Arc<Tracker>,
    parent_req_rows: Arc<AtomicI64>,
    max_chunk_size: usize,
}

impl ProjectionInputFetcher {
    /// run gets projectionInputFetcher's input and output resources from its
    /// "inputCh" and "outputCh" channel, once the input and output resources are
    /// abtained, it fetches child's result into "input.chk" and:
    ///
    ///  a. Dispatches this input to the worker specified in "input.targetWorker"
    ///  b. Dispatches this output to the main thread: "ProjectionExec.Next"
    ///  c. Dispatches this output to the worker specified in "input.targetWorker"
    ///
    /// It is finished and exited once:
    ///
    ///  a. There is no more input from child.
    ///  b. "ProjectionExec" close the "globalFinishCh"
    ///
    /// The fetcher is given back when the task ends (Close drains its channels and puts the
    /// child back).
    // go: projectionInputFetcher.run L2 (the deferred recover is a catch_unwind; no trace region; M-PANIC, M-WG)
    async fn run(mut self, ctx: ExecCtx) -> ProjectionInputFetcher {
        let mut output: Option<OutputRef> = None;
        let r = AssertUnwindSafe(self.run_loop(&ctx, &mut output))
            .catch_unwind()
            .await;
        if let Some(err) = recovered(r) {
            recovery_projection(output.as_ref(), err).await;
        }
        self.global_output_tx = None;
        self
    }

    /// The loop of `run`; an `Err` is a tracker's action (Go: a panic).
    // go-part: projectionInputFetcher.run
    async fn run_loop(
        &mut self,
        ctx: &ExecCtx,
        output: &mut Option<OutputRef>,
    ) -> Result<(), Error> {
        loop {
            let Some(mut input) =
                read_projection(&mut self.input_rx, &mut self.global_finish_rx).await
            else {
                return Ok(());
            };
            let target_worker = input.target_worker;

            *output = read_projection(&mut self.output_rx, &mut self.global_finish_rx).await;
            let Some(out) = output.clone() else {
                self.mem_tracker.consume(-input.chk.memory_usage())?;
                return Ok(());
            };

            if let Some(tx) = &self.global_output_tx {
                let _ = tx.send(Arc::clone(&out)).await;
            }

            let required_rows = self.parent_req_rows.load(Ordering::SeqCst);
            input
                .chk
                .set_required_rows(required_rows as isize, self.max_chunk_size);
            let m_size = input.chk.memory_usage();
            let err = exec::next(ctx, self.child.as_mut(), &mut input.chk).await;
            self.mem_tracker
                .consume(input.chk.memory_usage() - m_size)?;
            if err.is_err() || input.chk.num_rows() == 0 {
                let _ = out.done_tx.send(err).await;
                self.mem_tracker.consume(-input.chk.memory_usage())?;
                return Ok(());
            }

            let (input_tx, output_tx) = &self.workers[target_worker];
            let _ = input_tx.send(input).await;
            let _ = output_tx.send(out).await;
        }
    }
}

/// Go `projectionWorker`. Its `proj` gives it the memory tracker; the evaluation state is
/// its own (N4 §3.4).
pub(crate) struct ProjectionWorker {
    ctx: ProjectionExecutorContext,
    evaluator_suit: Arc<EvaluatorSuite>,
    global_finish_rx: watch::Receiver<bool>,
    input_give_back_tx: mpsc::Sender<ProjectionInput>,

    // channel "input" and "output" is :
    // a. initialized by "ProjectionExec.prepare"
    // b. written	  by "projectionInputFetcher.run"
    // c. read    	  by "projectionWorker.run"
    input_rx: mpsc::Receiver<ProjectionInput>,
    output_rx: mpsc::Receiver<OutputRef>,

    mem_tracker: Arc<Tracker>,
    eval_state: EvalState,
    rpn: RpnState,
}

impl ProjectionWorker {
    /// run gets projectionWorker's input and output resources from its
    /// "inputCh" and "outputCh" channel, once the input and output resources are
    /// abtained, it calculate the projection result use "input.chk" as the input
    /// and "output.chk" as the output, once the calculation is done, it:
    ///
    ///  a. Sends "nil" or error to "output.done" to mark this input is finished.
    ///  b. Returns the "input" resource to "projectionInputFetcher.inputCh".
    ///
    /// It is finished and exited once:
    ///
    ///  a. "ProjectionExec" closes the "globalFinishCh".
    ///
    /// The worker is given back when the task ends (Close drains its channels).
    // go: projectionWorker.run L2 (the deferred recover is a catch_unwind; no trace region; M-PANIC, M-WG)
    async fn run(mut self, _ctx: ExecCtx) -> ProjectionWorker {
        let mut output: Option<OutputRef> = None;
        let r = AssertUnwindSafe(self.run_loop(&mut output))
            .catch_unwind()
            .await;
        if let Some(err) = recovered(r) {
            recovery_projection(output.as_ref(), err).await;
        }
        self
    }

    /// The loop of `run`; an `Err` is a tracker's action (Go: a panic).
    // go-part: projectionWorker.run
    async fn run_loop(&mut self, output: &mut Option<OutputRef>) -> Result<(), Error> {
        loop {
            let Some(mut input) =
                read_projection(&mut self.input_rx, &mut self.global_finish_rx).await
            else {
                return Ok(());
            };

            *output = read_projection(&mut self.output_rx, &mut self.global_finish_rx).await;
            let Some(out) = output.clone() else {
                return Ok(());
            };

            let err = {
                let mut output_chk = out.chk.lock();
                let m_size = output_chk.memory_usage() + input.chk.memory_usage();
                let err = self.evaluator_suit.run(
                    &*self.ctx.eval_ctx,
                    &self.eval_state,
                    &mut self.rpn,
                    self.ctx.enable_vectorized_expression,
                    &mut input.chk,
                    &mut output_chk,
                );
                self.mem_tracker
                    .consume(output_chk.memory_usage() + input.chk.memory_usage() - m_size)?;
                err
            };
            let failed = err.is_err();
            let _ = out.done_tx.send(err).await;

            if failed {
                return Ok(());
            }

            let _ = self.input_give_back_tx.send(input).await;
        }
    }
}

/// What the deferred `recover()` of a goroutine gets: the panic, or the error of a tracker's
/// action (Go panics there too). `None`: the loop returned.
// glue: recover() of the deferred function (M-PANIC)
fn recovered(r: Result<Result<(), Error>, Box<dyn std::any::Any + Send>>) -> Option<Error> {
    match r {
        Ok(Ok(())) => None,
        Ok(Err(err)) => Some(err),
        Err(payload) => Some(get_recover_error(payload)),
    }
}

/// `util.GetRecoverError(r)`: the panic's message as an error.
// glue: recover() value to error (Go util.GetRecoverError)
fn get_recover_error(p: Box<dyn std::any::Any + Send>) -> Error {
    let msg = p
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
        .unwrap_or_else(|| "panic".to_string());
    Error::plain(msg)
}

/// Go logs the stack with `zap.Stack`; the log here has the error only.
// go: recoveryProjection L2 (the recovered value is already an error; no stack in the log)
async fn recovery_projection(output: Option<&OutputRef>, r: Error) {
    let msg = r.to_string();
    if let Some(output) = output {
        let _ = output.done_tx.send(Err(r)).await;
    }
    log::warn!("projection executor panicked, error={msg}");
}

/// The value a fetcher or worker task handed back. A task that ended without one (it
/// panicked outside `catch_unwind`, or the runtime is shutting down) is logged: the
/// fetcher's child is then lost and not closed. Go's goroutines cannot end this way.
// glue: Rust only (JoinHandle)
fn joined<T>(r: Result<T, tokio::task::JoinError>, task: &str) -> Option<T> {
    match r {
        Ok(v) => Some(v),
        Err(e) => {
            log::warn!("projection {task} task ended without returning, error={e}");
            None
        }
    }
}

/// A dropped `watch` sender (the executor dropped without `Close`) also reads as finished.
// go: readProjection L2 (the finish channel is a watch channel; M-CH)
async fn read_projection<T>(
    ch: &mut mpsc::Receiver<T>,
    finish_rx: &mut watch::Receiver<bool>,
) -> Option<T> {
    tokio::select! {
        _ = finish_rx.wait_for(|finished| *finished) => None,
        t = ch.recv() => t,
    }
}
