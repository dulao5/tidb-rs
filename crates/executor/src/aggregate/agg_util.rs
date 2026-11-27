//! go-file: pkg/executor/aggregate/agg_util.go
//!
//! The helpers of the aggregation executors (D12.3):
//! - Part A: `closeBaseExecutor`.
//! - Part B: `GetGroupKey`, `getBuffer`, `tryRecycleBuffer`, `recoveryHashAgg`,
//!   `getGroupKeyMemUsage`, `HashAggRuntimeStats` (`workerString`, `String`, `Clone`, `Merge`,
//!   `Tp`), `AggWorkerStat.Clone`, `HashAggExec.{actionSpillForUnparallel,
//!   actionSpillForParallel, ActionSpill}`, `updateWaitTime`, `updateWorkerTime`,
//!   `updateExecTime`, plus `murmur3::sum32` (`github.com/twmb/murmur3` v1.1.6).
//!
//! `failpointError` is reclassified as `X` (test hook, spec §2.4 / §8.2 decision 2) and not
//! ported.

// Go identifiers for buffer pool default capacities (M-GONAME).
#![allow(non_upper_case_globals)]

use std::any::Any;
use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Instant;

use tidb_base::Error;
use tidb_base::gostd::time::{Location, duration_string};
use tidb_base::mysql;
use tidb_base::terror;
use tidb_chunk::chunk::Chunk;
use tidb_codec::codec::hash_group_key;
use tidb_execdetails::execdetails::{RuntimeStats, TpHashAggRuntimeStat};
use tidb_expression::ExprRef;
use tidb_expression::context::EvalContext;
use tidb_expression::eval::RpnState;
use tidb_expression::eval::rt::empty_column;
use tidb_expression::expression::{EvalState, eval_expr};
use tidb_memory::action::ActionRef;
use tidb_sessionctx::errctx::Context as ErrCtx;
use tokio::sync::mpsc;

use crate::internal::exec::executor::BaseExecutor;
use crate::sortexec::sort_util::get_recover_error;

use super::agg_hash_executor::HashAggExec;
use super::agg_hash_final_worker::AfFinalResult;
use super::agg_spill::{AggSpillDiskAction, ParallelAggSpillDiskAction};

const defaultPartialResultsBufferCap: usize = 2048;
const defaultGroupKeyCap: usize = 8;
/// Go's `aggfuncs.DefSliceSize` (slice header size: ptr + len + cap = 24 bytes).
const defSliceSize: i64 = 24;

/// Go's `github.com/twmb/murmur3@v1.1.6` `Sum32` (used to route group keys to final workers
/// and spill partitions).
// go-dep: github.com/twmb/murmur3@v1.1.6 Sum32 L1
pub(crate) fn murmur3_sum32(data: &[u8]) -> u32 {
    const C1_32: u32 = 0xcc9e_2d51;
    const C2_32: u32 = 0x1b87_3593;

    let mut h1: u32 = 0;
    let clen = data.len() as u32;
    let (chunks, rem) = data.as_chunks::<4>();
    for &chunk in chunks {
        let mut k1 = u32::from_le_bytes(chunk);
        k1 = k1.wrapping_mul(C1_32);
        k1 = k1.rotate_left(15);
        k1 = k1.wrapping_mul(C2_32);

        h1 ^= k1;
        h1 = h1.rotate_left(13);
        h1 = h1.wrapping_mul(5).wrapping_add(0xe654_6b64);
    }

    let mut k1: u32 = 0;
    if rem.len() == 3 {
        k1 ^= u32::from(rem[2]) << 16;
    }
    if rem.len() >= 2 {
        k1 ^= u32::from(rem[1]) << 8;
    }
    if !rem.is_empty() {
        k1 ^= u32::from(rem[0]);
        k1 = k1.wrapping_mul(C1_32);
        k1 = k1.rotate_left(15);
        k1 = k1.wrapping_mul(C2_32);
        h1 ^= k1;
    }

    h1 ^= clen;
    h1 ^= h1 >> 16;
    h1 = h1.wrapping_mul(0x85eb_ca6b);
    h1 ^= h1 >> 13;
    h1 = h1.wrapping_mul(0xc2b2_ae35);
    h1 ^= h1 >> 16;

    h1
}

/// Allocates fresh per-worker scratch buffers (no `sync.Pool`, mapping rule M-POOL).
// go: getBuffer L1 [M-POOL]
pub(crate) fn get_buffer() -> (Vec<usize>, Vec<Vec<u8>>) {
    (
        Vec::with_capacity(defaultPartialResultsBufferCap),
        Vec::with_capacity(defaultGroupKeyCap),
    )
}

/// No-op under `M-POOL` (buffers are dropped with the worker).
// go: tryRecycleBuffer L1 [M-POOL]
pub(crate) fn try_recycle_buffer(_buf: &mut Vec<usize>, _group_key: &mut Vec<Vec<u8>>) {}

/// Go's deferred `closeBaseExecutor` recovers a panic of `OpenSelf`, closes the base
/// executor (the children) and panics again. A Go panic is an error here (M-PANIC), so the
/// caller calls it on the error path of `OpenSelf` and returns what it returns: `err`
/// itself, after the children were closed (spec §4.5).
// go: closeBaseExecutor L2 (recover → the error of OpenSelf, M-PANIC; async close)
pub(crate) async fn close_base_executor(b: &mut BaseExecutor, err: Error) -> Error {
    // Release the resource, but throw the panic again and let the top level handle it.
    terror::log(b.base.close().await.err().as_ref());
    tidb_base::log_warn!("panic in Open(), close base executor and throw exception again");
    err
}

/// Converts a caught task panic into an error, sends it to `output`, and logs a warning with
/// the captured stack trace (matching Go `agg_util.go:93`).
// go: recoveryHashAgg L2 (catch_unwind payload + captured stack; async mpsc send; M-PANIC, M-CH)
pub(crate) async fn recovery_hash_agg(
    output: &mpsc::Sender<AfFinalResult>,
    r: Box<dyn Any + Send>,
    bt: Option<String>,
) {
    let err = get_recover_error(r);
    let _ = output.send(AfFinalResult::err(err.clone())).await;
    let stack = bt.unwrap_or_else(|| {
        tidb_base::errors::format_backtrace_string(&std::backtrace::Backtrace::force_capture())
    });
    tidb_base::log_warn!(
        "parallel hash aggregation panicked",
        "error" => &err,
        "stack" => &stack,
    );
}

/// Computes the slice-capacity estimate of `groupKey` (kept L1 alongside A1 accounting).
// go: getGroupKeyMemUsage L1
#[allow(clippy::ptr_arg)] // Go reads cap(groupKey) on the outer slice.
pub(crate) fn get_group_key_mem_usage(group_key: &Vec<Vec<u8>>) -> i64 {
    let mut mem: i64 = 0;
    for key in group_key {
        mem += key.capacity() as i64;
    }
    mem += defSliceSize * group_key.capacity() as i64;
    mem
}

/// GetGroupKey evaluates the group items and encodes them as group-key byte strings.
///
/// Takes the specific context values Go reads from `sessionctx.Context` (N2): evaluation
/// context, error context, time zone, vectorized switch, and the executor's evaluation and
/// RPN scratch state (N4 §3.4). Temporary columns are allocated directly (M-POOL).
// go: GetGroupKey L2 (N2 explicit context fields; M-POOL no column pool; N4 EvalState/RpnState)
#[allow(clippy::too_many_arguments)]
pub fn get_group_key(
    eval_ctx: &dyn EvalContext,
    err_ctx: &ErrCtx,
    loc: &Arc<Location>,
    vec_enabled: bool,
    st: &EvalState,
    rpn: &mut RpnState,
    input: &Chunk,
    mut group_key: Vec<Vec<u8>>,
    group_by_items: &[ExprRef],
) -> Result<Vec<Vec<u8>>, Error> {
    let num_rows = input.num_rows();
    let avl_group_key_len = group_key.len().min(num_rows);
    for key in &mut group_key[..avl_group_key_len] {
        key.clear();
    }
    for _ in avl_group_key_len..num_rows {
        group_key.push(Vec::with_capacity(10 * group_by_items.len()));
    }

    for item in group_by_items {
        let mut tp = item.get_type(eval_ctx).into_owned();
        let mut buf = empty_column();

        // In strict sql mode like 'STRICT_TRANS_TABLES', cannot insert an invalid enum value
        // like 0. While in sql mode like '', can insert an invalid enum value like 0, then
        // the enum value 0 will have the enum name '', which may conflict with user defined
        // enum ''. Use enum value as groupKey instead of enum name.
        if tp.get_type() == mysql::TypeEnum {
            tp.add_flag(mysql::EnumSetAsIntFlag);
        }

        eval_expr(
            eval_ctx,
            st,
            rpn,
            vec_enabled,
            item,
            tp.eval_type(),
            input,
            &mut buf,
        )?;

        // Avoid error during the execution of `EncodeDecimal`.
        if tp.get_type() == mysql::TypeNewDecimal {
            tp.set_flen(0);
        }

        if let Err(err) = hash_group_key(loc, num_rows, &buf, &mut group_key[..num_rows], &tp) {
            err_ctx.handle_error(err)?;
        }
    }

    group_key.truncate(num_rows);
    Ok(group_key)
}

/// `AggWorkerInfo` contains the agg worker information.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AggWorkerInfo {
    pub concurrency: usize,
    pub wall_time: i64,
}

/// `AggWorkerStat` records runtime stats of one partial or final worker.
/// Fields are `AtomicI64` so workers update their own stats concurrently with `Close` reading
/// them (spec §4.1).
#[derive(Debug, Default)]
pub struct AggWorkerStat {
    pub task_num: AtomicI64,
    pub wait_time: AtomicI64,
    pub exec_time: AtomicI64,
    pub worker_time: AtomicI64,
}

impl AggWorkerStat {
    /// Creates an `AggWorkerStat` with the given values.
    // glue: Go composite literal &AggWorkerStat{...}
    pub fn new(task_num: i64, wait_time: i64, exec_time: i64, worker_time: i64) -> Self {
        Self {
            task_num: AtomicI64::new(task_num),
            wait_time: AtomicI64::new(wait_time),
            exec_time: AtomicI64::new(exec_time),
            worker_time: AtomicI64::new(worker_time),
        }
    }

    /// `Clone` implements the `AggWorkerStat.Clone` method.
    // go: AggWorkerStat.Clone L1
    pub fn clone_stat(&self) -> AggWorkerStat {
        AggWorkerStat::new(
            self.task_num.load(Ordering::Relaxed),
            self.wait_time.load(Ordering::Relaxed),
            self.exec_time.load(Ordering::Relaxed),
            self.worker_time.load(Ordering::Relaxed),
        )
    }
}

/// `HashAggRuntimeStats` records the `HashAggExec` runtime stats.
#[derive(Debug, Default)]
pub struct HashAggRuntimeStats {
    pub partial_concurrency: usize,
    pub partial_wall_time: AtomicI64,
    pub final_concurrency: usize,
    pub final_wall_time: AtomicI64,
    pub partial_stats: Vec<Arc<AggWorkerStat>>,
    pub final_stats: Vec<Arc<AggWorkerStat>>,
}

impl HashAggRuntimeStats {
    // go: HashAggRuntimeStats.workerString L2 (sorts a local copy of worker times instead of mutating in place, spec §6 item 7)
    fn worker_string(
        buf: &mut String,
        prefix: &str,
        concurrency: usize,
        wall_time: i64,
        worker_stats: &[Arc<AggWorkerStat>],
    ) {
        let mut total_time: i64 = 0;
        let mut total_wait: i64 = 0;
        let mut total_exec: i64 = 0;
        let mut total_task_num: i64 = 0;
        let mut worker_times = Vec::with_capacity(worker_stats.len());
        for w in worker_stats {
            let wt = w.worker_time.load(Ordering::Relaxed);
            total_time += wt;
            total_wait += w.wait_time.load(Ordering::Relaxed);
            total_exec += w.exec_time.load(Ordering::Relaxed);
            total_task_num += w.task_num.load(Ordering::Relaxed);
            worker_times.push(wt);
        }
        buf.push_str(prefix);
        let _ = write!(
            buf,
            "_worker:{{wall_time:{}, concurrency:{}, task_num:{}, tot_wait:{}, tot_exec:{}, tot_time:{}",
            duration_string(wall_time),
            concurrency,
            total_task_num,
            duration_string(total_wait),
            duration_string(total_exec),
            duration_string(total_time),
        );
        let n = worker_times.len();
        if n > 0 {
            worker_times.sort_unstable();
            let _ = write!(
                buf,
                ", max:{}, p95:{}",
                duration_string(worker_times[n - 1]),
                duration_string(worker_times[n * 19 / 20]),
            );
        }
        buf.push('}');
    }
}

impl RuntimeStats for HashAggRuntimeStats {
    /// `String` implements the `RuntimeStats` interface.
    // go: HashAggRuntimeStats.String L1
    fn string(&self) -> String {
        let mut buf = String::with_capacity(64);
        Self::worker_string(
            &mut buf,
            "partial",
            self.partial_concurrency,
            self.partial_wall_time.load(Ordering::SeqCst),
            &self.partial_stats,
        );
        buf.push_str(", ");
        Self::worker_string(
            &mut buf,
            "final",
            self.final_concurrency,
            self.final_wall_time.load(Ordering::SeqCst),
            &self.final_stats,
        );
        buf
    }

    /// `Clone` implements the `RuntimeStats` interface.
    // go: HashAggRuntimeStats.Clone L1
    fn clone_stats(&self) -> Box<dyn RuntimeStats> {
        let mut new_rs = HashAggRuntimeStats {
            partial_concurrency: self.partial_concurrency,
            partial_wall_time: AtomicI64::new(self.partial_wall_time.load(Ordering::SeqCst)),
            final_concurrency: self.final_concurrency,
            final_wall_time: AtomicI64::new(self.final_wall_time.load(Ordering::SeqCst)),
            partial_stats: Vec::with_capacity(self.partial_concurrency),
            final_stats: Vec::with_capacity(self.final_concurrency),
        };
        for s in &self.partial_stats {
            new_rs.partial_stats.push(Arc::new(s.clone_stat()));
        }
        for s in &self.final_stats {
            new_rs.final_stats.push(Arc::new(s.clone_stat()));
        }
        Box::new(new_rs)
    }

    /// `Merge` implements the `RuntimeStats` interface.
    // go: HashAggRuntimeStats.Merge L1
    fn merge(&mut self, other: &dyn RuntimeStats) {
        let Some(tmp) = other.as_any().downcast_ref::<HashAggRuntimeStats>() else {
            return;
        };
        self.partial_wall_time.fetch_add(
            tmp.partial_wall_time.load(Ordering::SeqCst),
            Ordering::SeqCst,
        );
        self.final_wall_time
            .fetch_add(tmp.final_wall_time.load(Ordering::SeqCst), Ordering::SeqCst);
        for s in &tmp.partial_stats {
            self.partial_stats.push(Arc::new(s.clone_stat()));
        }
        for s in &tmp.final_stats {
            self.final_stats.push(Arc::new(s.clone_stat()));
        }
    }

    /// `Tp` implements the `RuntimeStats` interface.
    // go: HashAggRuntimeStats.Tp L1
    fn tp(&self) -> i64 {
        TpHashAggRuntimeStat
    }

    // glue: type assertion support for RuntimeStats
    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl HashAggExec {
    // go: HashAggExec.actionSpillForUnparallel L1
    pub(crate) fn action_spill_for_unparallel(&mut self) -> ActionRef {
        let tracker = Arc::clone(&self.mem_tracker);
        let action = Arc::new(AggSpillDiskAction::new(
            Arc::clone(&self.in_spill_mode),
            tracker,
        ));
        self.spill_action = Some(Arc::clone(&action));
        action
    }

    // go: HashAggExec.actionSpillForParallel L1
    pub(crate) fn action_spill_for_parallel(&mut self) -> ActionRef {
        let tracker = Arc::clone(&self.mem_tracker);
        let spill_helper = Arc::clone(
            self.spill_helper
                .as_ref()
                .expect("initForParallelExec initialized spillHelper"),
        );
        let action = Arc::new(ParallelAggSpillDiskAction::new(tracker, spill_helper));
        self.parallel_agg_spill_action = Some(Arc::clone(&action));
        action
    }

    /// ActionSpill returns an action for spilling intermediate data for hashAgg.
    // go: HashAggExec.ActionSpill L1
    pub fn action_spill(&mut self) -> ActionRef {
        if self.is_unparallel_exec {
            return self.action_spill_for_unparallel();
        }
        self.action_spill_for_parallel()
    }
}

// go: updateWaitTime L1
pub(crate) fn update_wait_time(stats: Option<&AggWorkerStat>, start_time: Instant) {
    if let Some(stats) = stats {
        stats
            .wait_time
            .fetch_add(start_time.elapsed().as_nanos() as i64, Ordering::Relaxed);
    }
}

// go: updateWorkerTime L1
pub(crate) fn update_worker_time(stats: Option<&AggWorkerStat>, start_time: Instant) {
    if let Some(stats) = stats {
        stats
            .worker_time
            .fetch_add(start_time.elapsed().as_nanos() as i64, Ordering::Relaxed);
    }
}

// go: updateExecTime L1
pub(crate) fn update_exec_time(stats: Option<&AggWorkerStat>, start_time: Instant) {
    if let Some(stats) = stats {
        stats
            .exec_time
            .fetch_add(start_time.elapsed().as_nanos() as i64, Ordering::Relaxed);
        stats.task_num.fetch_add(1, Ordering::Relaxed);
    }
}
