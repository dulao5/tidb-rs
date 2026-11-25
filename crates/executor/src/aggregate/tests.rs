//! Tests of the stream aggregation executor (D12.3 part A) and its builder that are not Go
//! unit tests: the error path of `Open` (`closeBaseExecutor`, spec §4.5), the memory
//! accounting that replaces Go's `pendingMemDelta` batching (A1, spec §4.6; Go's
//! `TestStreamAggPendingMemDeltaBatching` counts the `Consume` calls of that batching, which
//! does not exist here), and the builder's `DefaultVal`. Go's `TestStreamAggRequiredRows` is
//! in `executor_required_rows_test.rs`; the row results are pinned by the govec domain
//! `aggexec`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use tidb_base::Error;
use tidb_base::blocked::blocked_by;
use tidb_base::mysql::{TypeDouble, TypeLonglong};
use tidb_chunk::chunk::{Chunk, new_chunk_with_capacity};
use tidb_expression::aggregation::{AggFuncDesc, AggFunctionMode, new_agg_func_desc};
use tidb_expression::ast_funcs as ast;
use tidb_expression::schema::new_schema;
use tidb_expression::{Column, Expr, ExprRef, Schema};
use tidb_memory::action::PanicOnExceed;
use tidb_memory::tracker::new_tracker;
use tidb_parser::types::FieldType;
use tidb_types::field_type::new_field_type;

use super::StreamAggExec;
use crate::adapter::TelemetryInfo;
use crate::adapter::tests::MockSession;
use crate::builder::new_executor_builder;
use crate::exec::ExecCtx;
use crate::glue::SessionContext;
use crate::internal::exec::executor::{
    self as exec, BaseExecutorV2, Executor, new_base_executor_v2,
};

/// A child returning one of its chunks per `Next`, then empty chunks; it fails at batch
/// `err_at` and records whether it was opened, closed, and dropped.
struct BatchSource {
    base: BaseExecutorV2,
    batches: Vec<Chunk>,
    ptr: usize,
    err_at: Option<usize>,
    opened: Arc<AtomicBool>,
    closed: Arc<AtomicBool>,
    dropped: Option<Arc<AtomicBool>>,
}

impl Drop for BatchSource {
    fn drop(&mut self) {
        if let Some(d) = &self.dropped {
            d.store(true, Ordering::SeqCst);
        }
    }
}

#[async_trait]
impl Executor for BatchSource {
    fn base(&self) -> &BaseExecutorV2 {
        &self.base
    }
    fn base_mut(&mut self) -> &mut BaseExecutorV2 {
        &mut self.base
    }
    async fn open(&mut self, _ctx: &ExecCtx) -> Result<(), Error> {
        self.ptr = 0;
        self.opened.store(true, Ordering::SeqCst);
        Ok(())
    }
    async fn next(&mut self, _ctx: &ExecCtx, req: &mut Chunk) -> Result<(), Error> {
        req.reset();
        if self.err_at == Some(self.ptr) {
            return Err(Error::plain("mock child error"));
        }
        if self.ptr >= self.batches.len() {
            return Ok(());
        }
        let b = &self.batches[self.ptr];
        req.append(b, 0, b.num_rows());
        self.ptr += 1;
        Ok(())
    }
    async fn close(&mut self) -> Result<(), Error> {
        self.closed.store(true, Ordering::SeqCst);
        Ok(())
    }
}

fn ctx() -> MockSession {
    let mut ctx = MockSession::new(1);
    let vars = &mut ctx.vars;
    vars.init_chunk_size = 32;
    vars.max_chunk_size = 1024;
    vars.stmt_ctx_mut().mem_tracker = Some(new_tracker(-1, -1));
    vars.track_aggregate_memory_usage = true;
    ctx
}

fn exec_ctx(ctx: &MockSession) -> ExecCtx {
    ExecCtx::new(ctx.vars.stmt_ctx_arc())
}

fn schema_of(types: &[FieldType]) -> Schema {
    new_schema(
        types
            .iter()
            .enumerate()
            .map(|(i, tp)| {
                Arc::new(Column {
                    unique_id: i as i64 + 1,
                    index: i as isize,
                    ret_type: Arc::new(tp.clone()),
                    ..Column::default()
                })
            })
            .collect(),
    )
}

fn col(tp: &FieldType, index: isize) -> ExprRef {
    Expr::Column(Column {
        index,
        ret_type: Arc::new(tp.clone()),
        ..Column::default()
    })
    .into_ref()
}

/// The child's columns: `(g BIGINT, v DOUBLE)`.
fn child_types() -> Vec<FieldType> {
    vec![new_field_type(TypeLonglong), new_field_type(TypeDouble)]
}

/// Chunks of `(g, v)` rows.
fn batches(rows: &[&[(i64, f64)]]) -> Vec<Chunk> {
    rows.iter()
        .map(|b| {
            let mut chk = new_chunk_with_capacity(&child_types(), b.len().max(1));
            for &(g, v) in *b {
                chk.append_int64(0, g);
                chk.append_float64(1, v);
            }
            chk
        })
        .collect()
}

struct Built {
    e: StreamAggExec,
    opened: Arc<AtomicBool>,
    closed: Arc<AtomicBool>,
}

fn desc(sctx: &MockSession, name: &str, arg: ExprRef, distinct: bool) -> AggFuncDesc {
    let ectx = sctx.get_expr_ctx();
    new_agg_func_desc(&*ectx, name, vec![arg], distinct).unwrap()
}

/// The executor over a `BatchSource`, with an output column per aggregate function.
fn build(
    sctx: &MockSession,
    input: Vec<Chunk>,
    err_at: Option<usize>,
    group_by: Vec<ExprRef>,
    aggs: &[AggFuncDesc],
) -> Built {
    let opened = Arc::new(AtomicBool::new(false));
    let closed = Arc::new(AtomicBool::new(false));
    let src = BatchSource {
        base: new_base_executor_v2(
            sctx.get_session_vars(),
            &sctx.sql_killer(),
            Some(schema_of(&child_types())),
            1,
            Vec::new(),
        ),
        batches: input,
        ptr: 0,
        err_at,
        opened: Arc::clone(&opened),
        closed: Arc::clone(&closed),
        dropped: None,
    };
    let out: Vec<FieldType> = aggs.iter().map(|d| (*d.base.ret_tp).clone()).collect();
    let mut b = new_executor_builder(sctx, None, Arc::new(TelemetryInfo::default()));
    let e = b
        .build_stream_agg_from_child_exec(
            sctx,
            Some(Box::new(src)),
            &schema_of(&out),
            2,
            &group_by,
            aggs,
        )
        .expect("built");
    assert!(b.err.is_none());
    Built { e, opened, closed }
}

/// Runs `e` to the end: the rows of every result chunk as strings, and the first error.
async fn run(ctx: &ExecCtx, e: &mut StreamAggExec) -> (Vec<String>, Option<Error>) {
    let mut out = Vec::new();
    let mut chk = exec::new_first_chunk(e);
    let types: Vec<FieldType> = exec::ret_types(e).iter().map(|t| (**t).clone()).collect();
    loop {
        if let Err(err) = e.next(ctx, &mut chk).await {
            return (out, Some(err));
        }
        if chk.num_rows() == 0 {
            return (out, None);
        }
        for i in 0..chk.num_rows() {
            let row = chk.get_row(i);
            let cells: Vec<String> = types
                .iter()
                .enumerate()
                .map(|(c, tp)| {
                    let d = row.get_datum(c, tp);
                    if d.is_null() {
                        "NULL".to_string()
                    } else {
                        String::from_utf8(d.to_string().unwrap()).unwrap()
                    }
                })
                .collect();
            out.push(cells.join(","));
        }
    }
}

#[tokio::test]
async fn groups_spanning_chunks() {
    let sctx = ctx();
    let ctx = exec_ctx(&sctx);
    let tps = child_types();
    let aggs = [
        desc(&sctx, ast::AggFuncCount, col(&tps[1], 1), false),
        desc(&sctx, ast::AggFuncSum, col(&tps[1], 1), false),
    ];
    let input = batches(&[&[(1, 1.0), (1, 2.0)], &[(1, 3.0), (2, 4.0)], &[(3, 5.0)]]);
    let mut b = build(&sctx, input, None, vec![col(&tps[0], 0)], &aggs);
    b.e.open(&ctx).await.unwrap();
    let (rows, err) = run(&ctx, &mut b.e).await;
    assert!(err.is_none());
    assert_eq!(rows, ["3,6", "1,4", "1,5"]);
    b.e.close().await.unwrap();
    assert!(b.closed.load(Ordering::SeqCst));
}

#[tokio::test]
async fn empty_input_default_value() {
    let sctx = ctx();
    let ctx = exec_ctx(&sctx);
    let tps = child_types();
    let count = desc(&sctx, ast::AggFuncCount, col(&tps[1], 1), false);
    let sum = desc(&sctx, ast::AggFuncSum, col(&tps[1], 1), false);
    let first = desc(&sctx, ast::AggFuncFirstRow, col(&tps[1], 1), false);

    // No GROUP BY, final (complete) mode: one row of the default values.
    let mut b = build(
        &sctx,
        Vec::new(),
        None,
        Vec::new(),
        &[count.clone(), sum.clone()],
    );
    assert!(b.e.default_val.is_some());
    b.e.open(&ctx).await.unwrap();
    assert_eq!(run(&ctx, &mut b.e).await.0, ["0,NULL"]);
    b.e.close().await.unwrap();

    // With GROUP BY, all FIRST_ROW, or a partial mode: no row.
    let mut partial = count.clone();
    partial.mode = AggFunctionMode::Partial1Mode;
    for (group_by, aggs) in [
        (vec![col(&tps[0], 0)], vec![count.clone()]),
        (Vec::new(), vec![first]),
        (Vec::new(), vec![partial]),
    ] {
        let mut b = build(&sctx, Vec::new(), None, group_by, &aggs);
        assert!(b.e.default_val.is_none());
        b.e.open(&ctx).await.unwrap();
        assert!(run(&ctx, &mut b.e).await.0.is_empty());
        b.e.close().await.unwrap();
    }
}

#[tokio::test]
async fn no_aggregate_function_counts_groups_as_virtual_rows() {
    let sctx = ctx();
    let ctx = exec_ctx(&sctx);
    let tps = child_types();
    let input = batches(&[&[(1, 1.0), (1, 2.0), (2, 3.0)], &[(2, 4.0), (5, 5.0)]]);
    let mut b = build(&sctx, input, None, vec![col(&tps[0], 0)], &[]);
    b.e.open(&ctx).await.unwrap();
    let mut chk = exec::new_first_chunk(&b.e);
    b.e.next(&ctx, &mut chk).await.unwrap();
    assert_eq!(chk.num_rows(), 3);
    b.e.next(&ctx, &mut chk).await.unwrap();
    assert_eq!(chk.num_rows(), 0);
    b.e.close().await.unwrap();
}

#[tokio::test]
async fn child_error_ends_the_execution() {
    let sctx = ctx();
    let ctx = exec_ctx(&sctx);
    let tps = child_types();
    let aggs = [desc(&sctx, ast::AggFuncCount, col(&tps[1], 1), false)];
    let input = batches(&[&[(1, 1.0), (2, 2.0)], &[(2, 3.0)]]);
    let mut b = build(&sctx, input, Some(1), vec![col(&tps[0], 0)], &aggs);
    b.e.open(&ctx).await.unwrap();
    let mut chk = exec::new_first_chunk(&b.e);
    let err = b.e.next(&ctx, &mut chk).await.unwrap_err();
    assert_eq!(err.to_string(), "mock child error");
    // `executed` is set: the next call returns nothing.
    b.e.next(&ctx, &mut chk).await.unwrap();
    assert_eq!(chk.num_rows(), 0);
    b.e.close().await.unwrap();
}

/// Go's `closeBaseExecutor`: when `OpenSelf` fails (Go: panics; here a nil aggregate
/// function, Go's nil pointer dereference), the children opened by `BaseExecutor.Open` are
/// closed before the error is returned. A successful `Open` leaves them open.
#[tokio::test]
async fn open_self_error_closes_the_children() {
    let sctx = ctx();
    let ctx = exec_ctx(&sctx);
    let tps = child_types();
    let aggs = [desc(&sctx, ast::AggFuncCount, col(&tps[1], 1), false)];

    let mut b = build(&sctx, Vec::new(), None, Vec::new(), &aggs);
    b.e.open(&ctx).await.unwrap();
    assert!(b.opened.load(Ordering::SeqCst));
    assert!(!b.closed.load(Ordering::SeqCst));
    b.e.close().await.unwrap();

    let mut b = build(&sctx, Vec::new(), None, Vec::new(), &aggs);
    b.e.agg_funcs.push(None);
    let err = b.e.open(&ctx).await.unwrap_err();
    assert_eq!(
        err.to_string(),
        "runtime error: invalid memory address or nil pointer dereference"
    );
    assert!(b.opened.load(Ordering::SeqCst));
    assert!(b.closed.load(Ordering::SeqCst));
}

/// The partial results' growth is accounted in the executor's tracker, under the statement's
/// when `tidb_track_aggregate_memory_usage` is on, and given back by `Close` (A1: Go's
/// estimated deltas and their 1 KiB batching are not kept).
#[tokio::test]
async fn memory_is_accounted_and_released() {
    for track in [true, false] {
        let mut sctx = ctx();
        sctx.vars.track_aggregate_memory_usage = track;
        let stmt_tracker = sctx.vars.stmt_ctx().mem_tracker.clone().unwrap();
        let ctx = exec_ctx(&sctx);
        let tps = child_types();
        let aggs = [desc(&sctx, ast::AggFuncSum, col(&tps[1], 1), true)];
        let rows: Vec<(i64, f64)> = (0..20_000).map(|i| (1, i as f64)).collect();
        let input: Vec<Chunk> = rows.chunks(1000).flat_map(|c| batches(&[c])).collect();
        let mut b = build(&sctx, input, None, vec![col(&tps[0], 0)], &aggs);
        b.e.open(&ctx).await.unwrap();
        let (out, err) = run(&ctx, &mut b.e).await;
        assert!(err.is_none());
        assert_eq!(out, ["199990000"]);
        let t = Arc::clone(b.e.mem_tracker.as_ref().unwrap());
        assert!(t.max_consumed() > 64 << 10, "{track}: {}", t.max_consumed());
        if track {
            assert!(stmt_tracker.max_consumed() > 64 << 10);
        } else {
            assert_eq!(stmt_tracker.max_consumed(), 0);
        }
        b.e.close().await.unwrap();
        assert_eq!(t.bytes_consumed(), 0, "{track}");
        assert_eq!(stmt_tracker.bytes_consumed(), 0, "{track}");

        // Opened again: the tracker is reset and reused.
        b.e.open(&ctx).await.unwrap();
        b.e.close().await.unwrap();
        assert_eq!(t.bytes_consumed(), 0, "{track}");
    }
}

/// Where Go consumes the pending delta (`consumeGroupRows`), the tracker's checkpoint runs the
/// statement's quota action: `Next` returns its error.
#[tokio::test]
async fn memory_quota_exceeded_in_a_group() {
    let sctx = ctx();
    let stmt_tracker = sctx.vars.stmt_ctx().mem_tracker.clone().unwrap();
    stmt_tracker.set_bytes_limit(32 << 10);
    stmt_tracker.set_action_on_exceed(Some(PanicOnExceed::new(Some(sctx.sql_killer()), 0)));
    let ctx = exec_ctx(&sctx);
    let tps = child_types();
    let aggs = [desc(&sctx, ast::AggFuncSum, col(&tps[1], 1), true)];
    let rows: Vec<(i64, f64)> = (0..20_000).map(|i| (1, i as f64)).collect();
    let input: Vec<Chunk> = rows.chunks(1000).flat_map(|c| batches(&[c])).collect();
    let mut b = build(&sctx, input, None, vec![col(&tps[0], 0)], &aggs);
    b.e.open(&ctx).await.unwrap();
    let (_, err) = run(&ctx, &mut b.e).await;
    let err = err.expect("the quota action's error");
    assert!(err.to_string().starts_with("[executor:8175]"), "{err}");
    b.e.close().await.unwrap();
}

#[tokio::test]
async fn build_errors() {
    let sctx = ctx();
    let tps = child_types();
    // An aggregate function of a later part of D12.4 (APPROX_COUNT_DISTINCT, part B2): `b.err`,
    // no executor.
    let gc = desc(
        &sctx,
        ast::AggFuncApproxCountDistinct,
        col(&tps[1], 1),
        false,
    );
    let src = BatchSource {
        base: new_base_executor_v2(
            sctx.get_session_vars(),
            &sctx.sql_killer(),
            Some(schema_of(&tps)),
            1,
            Vec::new(),
        ),
        batches: Vec::new(),
        ptr: 0,
        err_at: None,
        opened: Arc::default(),
        closed: Arc::default(),
        dropped: None,
    };
    let mut b = new_executor_builder(&sctx, None, Arc::new(TelemetryInfo::default()));
    let out = [(*gc.base.ret_tp).clone()];
    let e = b.build_stream_agg_from_child_exec(
        &sctx,
        Some(Box::new(src)),
        &schema_of(&out),
        2,
        &[],
        &[gc],
    );
    assert!(e.is_none());
    assert_eq!(blocked_by(b.err.as_ref().unwrap()), Some("D12.4"));
}

// ---------------------------------------------------------------- HashAgg (Part B1)

fn build_hash_agg(
    sctx: &MockSession,
    input: Vec<Chunk>,
    err_at: Option<usize>,
    group_by: Vec<ExprRef>,
    aggs: &[AggFuncDesc],
) -> (super::HashAggExec, Arc<AtomicBool>, Arc<AtomicBool>) {
    let opened = Arc::new(AtomicBool::new(false));
    let closed = Arc::new(AtomicBool::new(false));
    let src = BatchSource {
        base: new_base_executor_v2(
            sctx.get_session_vars(),
            &sctx.sql_killer(),
            Some(schema_of(&child_types())),
            1,
            Vec::new(),
        ),
        batches: input,
        ptr: 0,
        err_at,
        opened: Arc::clone(&opened),
        closed: Arc::clone(&closed),
        dropped: None,
    };
    let out: Vec<FieldType> = aggs.iter().map(|d| (*d.base.ret_tp).clone()).collect();
    let mut b = new_executor_builder(sctx, None, Arc::new(TelemetryInfo::default()));
    let e = b
        .build_hash_agg_from_child_exec(
            sctx,
            Some(Box::new(src)),
            &schema_of(&out),
            2,
            &group_by,
            aggs,
        )
        .expect("built hash agg");
    assert!(b.err.is_none());
    (e, opened, closed)
}

async fn run_exec(ctx: &ExecCtx, e: &mut dyn Executor) -> (Vec<String>, Option<Error>) {
    let mut out = Vec::new();
    let mut chk = exec::new_first_chunk(e);
    let types: Vec<FieldType> = exec::ret_types(e).iter().map(|t| (**t).clone()).collect();
    loop {
        if let Err(err) = e.next(ctx, &mut chk).await {
            return (out, Some(err));
        }
        if chk.num_rows() == 0 {
            return (out, None);
        }
        for i in 0..chk.num_rows() {
            let row = chk.get_row(i);
            let cells: Vec<String> = types
                .iter()
                .enumerate()
                .map(|(c, tp)| {
                    let d = row.get_datum(c, tp);
                    if d.is_null() {
                        "NULL".to_string()
                    } else {
                        String::from_utf8(d.to_string().unwrap()).unwrap()
                    }
                })
                .collect();
            out.push(cells.join(","));
        }
    }
}

fn unparallel_ctx() -> MockSession {
    let mut sctx = ctx();
    sctx.vars.set_hash_agg_partial_concurrency(1);
    sctx.vars.set_hash_agg_final_concurrency(1);
    sctx
}

#[tokio::test]
async fn hash_agg_unparallel_groups_and_order() {
    let sctx = unparallel_ctx();
    let ctx = exec_ctx(&sctx);
    let tps = child_types();
    let aggs = [
        desc(&sctx, ast::AggFuncFirstRow, col(&tps[0], 0), false),
        desc(&sctx, ast::AggFuncCount, col(&tps[1], 1), false),
        desc(&sctx, ast::AggFuncSum, col(&tps[1], 1), false),
    ];
    // Groups arrive interleaved across chunks; unparallel HashAgg emits in first-seen order.
    let input = batches(&[
        &[(2, 10.0), (1, 1.0), (2, 20.0)],
        &[(3, 5.0), (1, 2.0), (1, 3.0)],
    ]);
    let (mut e, _, closed) = build_hash_agg(&sctx, input, None, vec![col(&tps[0], 0)], &aggs);
    assert!(e.is_unparallel_exec);
    e.open(&ctx).await.unwrap();
    let (rows, err) = run_exec(&ctx, &mut e).await;
    assert!(err.is_none());
    assert_eq!(rows, ["2,2,30", "1,3,6", "3,1,5"]);
    e.close().await.unwrap();
    assert!(closed.load(Ordering::SeqCst));
    assert_eq!(e.mem_tracker.bytes_consumed(), 0);
}

#[tokio::test]
async fn hash_agg_unparallel_empty_input_and_distinct() {
    let sctx = unparallel_ctx();
    let ectx = exec_ctx(&sctx);
    let tps = child_types();
    let count = desc(&sctx, ast::AggFuncCount, col(&tps[1], 1), false);
    let sum_distinct = desc(&sctx, ast::AggFuncSum, col(&tps[1], 1), true);

    // No GROUP BY, empty input: emits empty-key default row.
    let (mut e, _, _) = build_hash_agg(
        &sctx,
        Vec::new(),
        None,
        Vec::new(),
        std::slice::from_ref(&count),
    );
    e.open(&ectx).await.unwrap();
    assert_eq!(run_exec(&ectx, &mut e).await.0, ["0"]);
    e.close().await.unwrap();

    // With GROUP BY, empty input: no rows.
    let (mut e, _, _) = build_hash_agg(&sctx, Vec::new(), None, vec![col(&tps[0], 0)], &[count]);
    e.open(&ectx).await.unwrap();
    assert!(run_exec(&ectx, &mut e).await.0.is_empty());
    e.close().await.unwrap();

    // DISTINCT forces unparallel even when concurrency > 1.
    let mut par_sctx = ctx();
    par_sctx.vars.set_hash_agg_partial_concurrency(4);
    par_sctx.vars.set_hash_agg_final_concurrency(4);
    let par_ctx = exec_ctx(&par_sctx);
    let input = batches(&[&[(1, 2.0), (1, 2.0), (1, 5.0)], &[(2, 3.0), (2, 3.0)]]);
    let (mut e, _, _) = build_hash_agg(
        &par_sctx,
        input,
        None,
        vec![col(&tps[0], 0)],
        &[sum_distinct],
    );
    assert!(e.is_unparallel_exec);
    e.open(&par_ctx).await.unwrap();
    let (rows, err) = run_exec(&par_ctx, &mut e).await;
    assert!(err.is_none());
    assert_eq!(rows, ["7", "3"]);
    e.close().await.unwrap();
    assert_eq!(e.mem_tracker.bytes_consumed(), 0);
}

#[tokio::test]
async fn hash_agg_unparallel_spill_action_and_boundary() {
    use std::sync::atomic::AtomicUsize;
    use tidb_memory::action::{ActionOnExceed, BaseOOMAction, DefSpillPriority};
    use tidb_memory::tracker::Tracker;

    // 1. Verify AggSpillDiskAction: 20% threshold, maxSpillTimes, fallback, priority.
    struct CountFallback {
        base: BaseOOMAction,
        hits: Arc<AtomicUsize>,
    }
    impl ActionOnExceed for CountFallback {
        fn action(&self, _t: &Arc<Tracker>) -> Result<(), Error> {
            self.hits.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn set_fallback(&self, a: Option<tidb_memory::action::ActionRef>) {
            self.base.set_fallback(a)
        }
        fn get_fallback(&self) -> Option<tidb_memory::action::ActionRef> {
            self.base.get_fallback()
        }
        fn get_priority(&self) -> i64 {
            0
        }
        fn set_finished(&self) {
            self.base.set_finished()
        }
        fn is_finished(&self) -> bool {
            self.base.is_finished()
        }
    }

    let in_spill = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let agg_t = new_tracker(2, -1);
    let sess_t = new_tracker(1, 1000);
    let action = Arc::new(super::agg_spill::AggSpillDiskAction::new(
        Arc::clone(&in_spill),
        Arc::clone(&agg_t),
    ));
    let fb_hits = Arc::new(AtomicUsize::new(0));
    let fb = Arc::new(CountFallback {
        base: BaseOOMAction::default(),
        hits: Arc::clone(&fb_hits),
    });
    action.set_fallback(Some(fb));
    assert_eq!(action.get_priority(), DefSpillPriority);

    // Below 20% (199 < 200): falls back instead of entering spill mode.
    agg_t.record(199);
    action.action(&sess_t).unwrap();
    assert_eq!(in_spill.load(Ordering::SeqCst), 0);
    assert_eq!(fb_hits.load(Ordering::SeqCst), 1);

    // At >= 20% (200 >= 200): enters spill mode up to maxSpillTimes (10), then falls back.
    agg_t.record(1);
    for _ in 0..super::agg_spill::maxSpillTimes {
        in_spill.store(0, Ordering::SeqCst);
        action.action(&sess_t).unwrap();
        assert_eq!(in_spill.load(Ordering::SeqCst), 1);
    }
    assert_eq!(fb_hits.load(Ordering::SeqCst), 1);
    in_spill.store(0, Ordering::SeqCst);
    action.action(&sess_t).unwrap();
    assert_eq!(in_spill.load(Ordering::SeqCst), 0);
    assert_eq!(fb_hits.load(Ordering::SeqCst), 2);

    // 2. End-to-end unparallel spill boundary: soft limit triggers inSpillMode ->
    //    DataInDiskByChunks.Add returns blocked-by:D01.10 with zero rows emitted and
    //    tracker back to 0 on Close.
    let mut sctx = unparallel_ctx();
    let sess_tracker = new_tracker(1, 16 << 10);
    let stmt_tracker = new_tracker(2, -1);
    stmt_tracker.attach_to(&sess_tracker).unwrap();
    sctx.vars.stmt_ctx_mut().mem_tracker = Some(Arc::clone(&stmt_tracker));
    let ctx = exec_ctx(&sctx);
    let tps = child_types();
    let aggs = [desc(&sctx, ast::AggFuncSum, col(&tps[1], 1), false)];
    let rows: Vec<(i64, f64)> = (0..2000).map(|i| (i, i as f64)).collect();
    let input: Vec<Chunk> = rows.chunks(64).flat_map(|c| batches(&[c])).collect();
    let (mut e, _, _) = build_hash_agg(&sctx, input, None, vec![col(&tps[0], 0)], &aggs);
    e.session_mem_tracker = Some(Arc::clone(&sess_tracker));
    e.open(&ctx).await.unwrap();
    let (out, err) = run_exec(&ctx, &mut e).await;
    assert!(out.is_empty(), "must emit zero rows before spill error");
    let err = err.expect("spill must stop at D01.10 boundary");
    assert_eq!(blocked_by(&err), Some("D01.10"), "{err}");
    e.close().await.unwrap();
    assert_eq!(e.mem_tracker.bytes_consumed(), 0);
    assert_eq!(stmt_tracker.bytes_consumed(), 0);
    assert_eq!(sess_tracker.bytes_consumed(), 0);
}

// ---------------------------------------------------------------- HashAgg (Part B2)

fn parallel_ctx(partial: i64, final_c: i64) -> MockSession {
    let mut sctx = ctx();
    sctx.vars.set_hash_agg_partial_concurrency(partial);
    sctx.vars.set_hash_agg_final_concurrency(final_c);
    sctx.vars.enable_parallel_hashagg_spill = true;
    sctx
}

/// Port of Go's `TestHashAggRuntimeStat` (`pkg/executor/test/aggregate/aggregate_test.go:38`).
#[test]
fn test_hash_agg_runtime_stat() {
    use tidb_execdetails::execdetails::{RuntimeStats, TpHashAggRuntimeStat};

    use super::{AggWorkerInfo, AggWorkerStat, HashAggRuntimeStats};

    const SEC: i64 = 1_000_000_000;
    const MSEC: i64 = 1_000_000;

    let partial_info = AggWorkerInfo {
        concurrency: 5,
        wall_time: 0,
    };
    let final_info = AggWorkerInfo {
        concurrency: 8,
        wall_time: 0,
    };
    let mut stats = HashAggRuntimeStats {
        partial_concurrency: 5,
        partial_wall_time: std::sync::atomic::AtomicI64::new(20 * SEC),
        final_concurrency: 8,
        final_wall_time: std::sync::atomic::AtomicI64::new(10 * SEC),
        partial_stats: Vec::new(),
        final_stats: Vec::new(),
    };
    for i in 0..partial_info.concurrency as i64 {
        stats
            .partial_stats
            .push(Arc::new(AggWorkerStat::new(5, 2 * SEC, SEC, i * SEC)));
    }
    for i in 0..final_info.concurrency as i64 {
        stats
            .final_stats
            .push(Arc::new(AggWorkerStat::new(5, 2 * MSEC, MSEC, i * MSEC)));
    }
    assert_eq!(stats.tp(), TpHashAggRuntimeStat);
    let expect = "partial_worker:{wall_time:20s, concurrency:5, task_num:25, tot_wait:10s, tot_exec:5s, tot_time:10s, max:4s, p95:4s}, final_worker:{wall_time:10s, concurrency:8, task_num:40, tot_wait:16ms, tot_exec:8ms, tot_time:28ms, max:7ms, p95:7ms}";
    assert_eq!(stats.string(), expect);
    let cloned = stats.clone_stats();
    assert_eq!(cloned.string(), expect);
    stats.merge(cloned.as_ref());
    let expect_merged = "partial_worker:{wall_time:40s, concurrency:5, task_num:50, tot_wait:20s, tot_exec:10s, tot_time:20s, max:4s, p95:4s}, final_worker:{wall_time:20s, concurrency:8, task_num:80, tot_wait:32ms, tot_exec:16ms, tot_time:56ms, max:7ms, p95:7ms}";
    assert_eq!(stats.string(), expect_merged);
}

/// Executor-level port of Go's `TestParallelHashAgg` case-variant keys (`aggregate_test.go:388`).
#[tokio::test]
async fn test_parallel_hash_agg_case_variants() {
    use tidb_base::mysql::TypeVarchar;

    let mut sctx = parallel_ctx(4, 4);
    sctx.vars.max_chunk_size = 32;
    let ectx = exec_ctx(&sctx);

    let mut str_ft = new_field_type(TypeVarchar);
    str_ft.set_flen(30);
    str_ft.set_charset("utf8mb4");
    str_ft.set_collate("utf8mb4_bin");
    let int_ft = new_field_type(TypeLonglong);
    let src_types = vec![str_ft.clone(), int_ft.clone()];

    let keys = [
        "aa", "AA", "aA", "Aa", "bb", "BB", "bB", "Bb", "cc", "CC", "cC", "Cc", "dd", "DD", "dD",
        "Dd", "ee", "EE", "eE", "Ee",
    ];
    let mut input = Vec::new();
    for _ in 0..20 {
        let mut chk = new_chunk_with_capacity(&src_types, keys.len());
        for k in keys {
            chk.append_string(0, k.as_bytes());
            chk.append_int64(1, 1);
        }
        input.push(chk);
    }

    let src = BatchSource {
        base: new_base_executor_v2(
            sctx.get_session_vars(),
            &sctx.sql_killer(),
            Some(schema_of(&src_types)),
            1,
            Vec::new(),
        ),
        batches: input,
        ptr: 0,
        err_at: None,
        opened: Arc::new(AtomicBool::new(false)),
        closed: Arc::new(AtomicBool::new(false)),
        dropped: None,
    };
    let mut sum_desc = desc(&sctx, ast::AggFuncSum, col(&int_ft, 1), false);
    sum_desc
        .wrap_cast_for_agg_args(&*sctx.get_expr_ctx())
        .unwrap();
    let aggs = [
        desc(&sctx, ast::AggFuncFirstRow, col(&str_ft, 0), false),
        sum_desc,
    ];
    let out: Vec<FieldType> = aggs.iter().map(|d| (*d.base.ret_tp).clone()).collect();
    let mut b = new_executor_builder(&sctx, None, Arc::new(TelemetryInfo::default()));
    let mut e = b
        .build_hash_agg_from_child_exec(
            &sctx,
            Some(Box::new(src)),
            &schema_of(&out),
            2,
            &[col(&str_ft, 0)],
            &aggs,
        )
        .expect("built parallel hash agg");
    assert!(!e.is_unparallel_exec);
    e.open(&ectx).await.unwrap();
    let (mut rows, err) = run_exec(&ectx, &mut e).await;
    assert!(err.is_none());
    e.close().await.unwrap();
    assert_eq!(e.mem_tracker.bytes_consumed(), 0);

    rows.sort();
    let mut expected: Vec<String> = keys.iter().map(|k| format!("{k},20")).collect();
    expected.sort();
    assert_eq!(rows, expected);
}

#[tokio::test]
async fn hash_agg_parallel_multi_agg_avg_empty_and_stats() {
    use tidb_execdetails::execdetails::RuntimeStatsColl;

    let mut sctx = parallel_ctx(3, 3);
    let coll = RuntimeStatsColl::new(None);
    sctx.vars.stmt_ctx_mut().runtime_stats_coll = Some(Arc::clone(&coll));
    let ectx = exec_ctx(&sctx);
    let tps = child_types();
    let aggs = [
        desc(&sctx, ast::AggFuncFirstRow, col(&tps[0], 0), false),
        desc(&sctx, ast::AggFuncCount, col(&tps[1], 1), false),
        desc(&sctx, ast::AggFuncSum, col(&tps[1], 1), false),
        desc(&sctx, ast::AggFuncAvg, col(&tps[1], 1), false),
        desc(&sctx, ast::AggFuncMin, col(&tps[1], 1), false),
        desc(&sctx, ast::AggFuncMax, col(&tps[1], 1), false),
    ];

    let input = batches(&[
        &[(1, 10.0), (2, 4.0), (1, 20.0)],
        &[(3, 7.0), (2, 6.0), (1, 30.0)],
    ]);
    let (mut e, _, closed) = build_hash_agg(&sctx, input, None, vec![col(&tps[0], 0)], &aggs);
    assert!(!e.is_unparallel_exec);
    e.open(&ectx).await.unwrap();
    let (mut rows, err) = run_exec(&ectx, &mut e).await;
    assert!(err.is_none());
    e.close().await.unwrap();
    assert!(closed.load(Ordering::SeqCst));
    assert_eq!(e.mem_tracker.bytes_consumed(), 0);

    rows.sort();
    assert_eq!(rows, ["1,3,60,20,10,30", "2,2,10,5,4,6", "3,1,7,7,7,7"]);
    // Runtime stats were registered in Close.
    let stat_str = coll.get_root_stats(2).string();
    assert!(
        stat_str.contains("partial_worker:") && stat_str.contains("final_worker:"),
        "{stat_str}"
    );

    // Empty input without GROUP BY emits DefaultVal.
    let count = desc(&sctx, ast::AggFuncCount, col(&tps[1], 1), false);
    let sum = desc(&sctx, ast::AggFuncSum, col(&tps[1], 1), false);
    let (mut e, _, _) = build_hash_agg(&sctx, Vec::new(), None, Vec::new(), &[count, sum]);
    e.open(&ectx).await.unwrap();
    let (rows, err) = run_exec(&ectx, &mut e).await;
    assert!(err.is_none());
    assert_eq!(rows, ["0,NULL"]);
    e.close().await.unwrap();
    assert_eq!(e.mem_tracker.bytes_consumed(), 0);
}

/// Child that panics on its `panic_at`-th `Next` call, and optionally fails `Close`.
struct FaultySource {
    base: BaseExecutorV2,
    batches: Vec<Chunk>,
    ptr: usize,
    panic_at: Option<usize>,
    close_err: bool,
    closed: Arc<AtomicBool>,
}

#[async_trait]
impl Executor for FaultySource {
    fn base(&self) -> &BaseExecutorV2 {
        &self.base
    }
    fn base_mut(&mut self) -> &mut BaseExecutorV2 {
        &mut self.base
    }
    async fn open(&mut self, _ctx: &ExecCtx) -> Result<(), Error> {
        Ok(())
    }
    async fn next(&mut self, _ctx: &ExecCtx, req: &mut Chunk) -> Result<(), Error> {
        req.reset();
        if self.panic_at == Some(self.ptr) {
            panic!("mock child panic");
        }
        if self.ptr >= self.batches.len() {
            return Ok(());
        }
        let b = &self.batches[self.ptr];
        req.append(b, 0, b.num_rows());
        self.ptr += 1;
        Ok(())
    }
    async fn close(&mut self) -> Result<(), Error> {
        self.closed.store(true, Ordering::SeqCst);
        if self.close_err {
            return Err(Error::plain("mock close error"));
        }
        Ok(())
    }
}

/// Drop guards under all 4 outcomes (normal, child error, child panic, memory quota
/// exceeded) plus stats registration on Close error (spec §4.5, §5.3).
#[tokio::test]
async fn hash_agg_parallel_error_panic_quota_and_close_stats() {
    use tidb_execdetails::execdetails::RuntimeStatsColl;

    let tps = child_types();

    // 1. Child error mid-stream.
    {
        let sctx = parallel_ctx(3, 3);
        let ectx = exec_ctx(&sctx);
        let aggs = [desc(&sctx, ast::AggFuncCount, col(&tps[1], 1), false)];
        let input = batches(&[&[(1, 1.0)], &[(2, 2.0)], &[(3, 3.0)]]);
        let (mut e, _, closed) =
            build_hash_agg(&sctx, input, Some(1), vec![col(&tps[0], 0)], &aggs);
        e.open(&ectx).await.unwrap();
        let (_, err) = run_exec(&ectx, &mut e).await;
        assert_eq!(err.unwrap().to_string(), "mock child error");
        e.close().await.unwrap();
        assert!(closed.load(Ordering::SeqCst));
        assert_eq!(e.mem_tracker.bytes_consumed(), 0);
    }

    // 2. Child panic mid-stream -> recovered via recoveryHashAgg, workers exit, Close works
    //    and registers stats even if child Close fails.
    {
        let mut sctx = parallel_ctx(2, 2);
        let coll = RuntimeStatsColl::new(None);
        sctx.vars.stmt_ctx_mut().runtime_stats_coll = Some(Arc::clone(&coll));
        let ectx = exec_ctx(&sctx);
        let aggs = [desc(&sctx, ast::AggFuncSum, col(&tps[1], 1), false)];
        let closed = Arc::new(AtomicBool::new(false));
        let src = FaultySource {
            base: new_base_executor_v2(
                sctx.get_session_vars(),
                &sctx.sql_killer(),
                Some(schema_of(&tps)),
                1,
                Vec::new(),
            ),
            batches: batches(&[&[(1, 1.0)], &[(2, 2.0)]]),
            ptr: 0,
            panic_at: Some(1),
            close_err: true,
            closed: Arc::clone(&closed),
        };
        let out: Vec<FieldType> = aggs.iter().map(|d| (*d.base.ret_tp).clone()).collect();
        let mut b = new_executor_builder(&sctx, None, Arc::new(TelemetryInfo::default()));
        let mut e = b
            .build_hash_agg_from_child_exec(
                &sctx,
                Some(Box::new(src)),
                &schema_of(&out),
                7,
                &[col(&tps[0], 0)],
                &aggs,
            )
            .unwrap();
        e.open(&ectx).await.unwrap();
        let (_, err) = run_exec(&ectx, &mut e).await;
        assert_eq!(err.unwrap().to_string(), "mock child panic");
        let close_err = e.close().await.unwrap_err();
        assert_eq!(close_err.to_string(), "mock close error");
        assert!(closed.load(Ordering::SeqCst));
        assert_eq!(e.mem_tracker.bytes_consumed(), 0);
        // Stats were registered despite Close returning an error (defer RegisterStats).
        assert!(coll.get_root_stats(7).string().contains("partial_worker:"));
    }

    // 3. Hard memory quota exceeded (PanicOnExceed) in parallel mode -> [executor:8175].
    {
        let sctx = parallel_ctx(2, 2);
        let stmt_tracker = sctx.vars.stmt_ctx().mem_tracker.clone().unwrap();
        stmt_tracker.set_bytes_limit(32 << 10);
        stmt_tracker.set_action_on_exceed(Some(PanicOnExceed::new(Some(sctx.sql_killer()), 0)));
        let ectx = exec_ctx(&sctx);
        let aggs = [desc(&sctx, ast::AggFuncSum, col(&tps[1], 1), false)];
        let rows: Vec<(i64, f64)> = (0..5000).map(|i| (i, i as f64)).collect();
        let input: Vec<Chunk> = rows.chunks(64).flat_map(|c| batches(&[c])).collect();
        let (mut e, _, closed) = build_hash_agg(&sctx, input, None, vec![col(&tps[0], 0)], &aggs);
        e.open(&ectx).await.unwrap();
        let (_, err) = run_exec(&ectx, &mut e).await;
        let err = err.expect("quota error");
        assert!(err.to_string().starts_with("[executor:8175]"), "{err}");
        e.close().await.unwrap();
        assert!(closed.load(Ordering::SeqCst));
        assert_eq!(e.mem_tracker.bytes_consumed(), 0);
        assert_eq!(stmt_tracker.bytes_consumed(), 0);
    }
}

/// Lifecycle edge cases (spec §5.3): Close before Next, early Close mid-stream, Drop without
/// Close, and Close future dropped mid-await.
#[tokio::test]
async fn hash_agg_parallel_lifecycle_and_cancellation() {
    use std::pin::Pin;
    use std::task::{Context, Waker};

    let tps = child_types();

    // 1. Close after Open without calling Next.
    {
        let sctx = parallel_ctx(4, 4);
        let ectx = exec_ctx(&sctx);
        let aggs = [desc(&sctx, ast::AggFuncCount, col(&tps[1], 1), false)];
        let (mut e, _, closed) = build_hash_agg(
            &sctx,
            batches(&[&[(1, 1.0)]]),
            None,
            vec![col(&tps[0], 0)],
            &aggs,
        );
        e.open(&ectx).await.unwrap();
        e.close().await.unwrap();
        assert!(closed.load(Ordering::SeqCst));
        assert_eq!(e.mem_tracker.bytes_consumed(), 0);
    }

    // 2. Early Close after reading only the first chunk.
    {
        let mut sctx = parallel_ctx(3, 3);
        sctx.vars.init_chunk_size = 4;
        sctx.vars.max_chunk_size = 4;
        let ectx = exec_ctx(&sctx);
        let aggs = [desc(&sctx, ast::AggFuncSum, col(&tps[1], 1), false)];
        let rows: Vec<(i64, f64)> = (0..500).map(|i| (i, i as f64)).collect();
        let input: Vec<Chunk> = rows.chunks(32).flat_map(|c| batches(&[c])).collect();
        let (mut e, _, closed) = build_hash_agg(&sctx, input, None, vec![col(&tps[0], 0)], &aggs);
        e.open(&ectx).await.unwrap();
        let mut chk = exec::new_first_chunk(&e);
        e.next(&ectx, &mut chk).await.unwrap();
        assert!(chk.num_rows() > 0);
        e.close().await.unwrap();
        assert!(closed.load(Ordering::SeqCst));
        assert_eq!(e.mem_tracker.bytes_consumed(), 0);
    }

    // 3. Drop without Close after Next started tasks -> ParallelTasks::drop aborts tasks,
    //    drops child executor, and releases all worker Arc references to mem_tracker.
    {
        let mut sctx = parallel_ctx(2, 2);
        sctx.vars.init_chunk_size = 2;
        sctx.vars.max_chunk_size = 2;
        let ectx = exec_ctx(&sctx);
        let aggs = [desc(&sctx, ast::AggFuncSum, col(&tps[1], 1), false)];
        let rows: Vec<(i64, f64)> = (0..200).map(|i| (i, i as f64)).collect();
        let input: Vec<Chunk> = rows.chunks(16).flat_map(|c| batches(&[c])).collect();
        let dropped = Arc::new(AtomicBool::new(false));
        let opened = Arc::new(AtomicBool::new(false));
        let closed = Arc::new(AtomicBool::new(false));
        let src = BatchSource {
            base: new_base_executor_v2(
                sctx.get_session_vars(),
                &sctx.sql_killer(),
                Some(schema_of(&tps)),
                1,
                Vec::new(),
            ),
            batches: input,
            ptr: 0,
            err_at: None,
            opened,
            closed,
            dropped: Some(Arc::clone(&dropped)),
        };
        let out_schema = schema_of(&[(*aggs[0].base.ret_tp).clone()]);
        let mut b = new_executor_builder(&sctx, None, Arc::new(TelemetryInfo::default()));
        let mut e = b
            .build_hash_agg_from_child_exec(
                &sctx,
                Some(Box::new(src)),
                &out_schema,
                2,
                &[col(&tps[0], 0)],
                &aggs,
            )
            .unwrap();
        let tracker = Arc::clone(&e.mem_tracker);
        e.open(&ectx).await.unwrap();
        let mut chk = exec::new_first_chunk(&e);
        e.next(&ectx, &mut chk).await.unwrap();
        assert!(chk.num_rows() > 0);
        // Drop the executor without calling Close; also drop ectx/sctx so stmt_tracker releases
        // its child reference, leaving only our local `tracker` clone once all tasks exit.
        drop(e);
        drop(ectx);
        drop(sctx);
        for _ in 0..100 {
            if dropped.load(Ordering::SeqCst) && Arc::strong_count(&tracker) == 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
        assert!(
            dropped.load(Ordering::SeqCst),
            "child executor must be dropped when tasks abort"
        );
        assert_eq!(
            Arc::strong_count(&tracker),
            1,
            "all spawned worker/fetcher tasks must exit and drop their tracker handles"
        );
    }

    // 4. Close future dropped mid-await -> remaining tasks aborted on Drop (or completed on
    //    a subsequent Close).
    {
        let mut sctx = parallel_ctx(2, 2);
        sctx.vars.init_chunk_size = 2;
        sctx.vars.max_chunk_size = 2;
        let ectx = exec_ctx(&sctx);
        let aggs = [desc(&sctx, ast::AggFuncSum, col(&tps[1], 1), false)];
        let rows: Vec<(i64, f64)> = (0..200).map(|i| (i, i as f64)).collect();
        let input: Vec<Chunk> = rows.chunks(16).flat_map(|c| batches(&[c])).collect();
        let dropped = Arc::new(AtomicBool::new(false));
        let opened = Arc::new(AtomicBool::new(false));
        let closed = Arc::new(AtomicBool::new(false));
        let src = BatchSource {
            base: new_base_executor_v2(
                sctx.get_session_vars(),
                &sctx.sql_killer(),
                Some(schema_of(&tps)),
                1,
                Vec::new(),
            ),
            batches: input,
            ptr: 0,
            err_at: None,
            opened,
            closed: Arc::clone(&closed),
            dropped: Some(Arc::clone(&dropped)),
        };
        let out_schema = schema_of(&[(*aggs[0].base.ret_tp).clone()]);
        let mut b = new_executor_builder(&sctx, None, Arc::new(TelemetryInfo::default()));
        let mut e = b
            .build_hash_agg_from_child_exec(
                &sctx,
                Some(Box::new(src)),
                &out_schema,
                2,
                &[col(&tps[0], 0)],
                &aggs,
            )
            .unwrap();
        let tracker = Arc::clone(&e.mem_tracker);
        e.open(&ectx).await.unwrap();
        let mut chk = exec::new_first_chunk(&e);
        e.next(&ectx, &mut chk).await.unwrap();
        {
            let mut close_fut = e.close();
            let waker = Waker::noop();
            let mut cx = Context::from_waker(waker);
            let _ = Pin::as_mut(&mut close_fut).poll(&mut cx);
        }
        // Complete Close after cancelling the first Close future mid-await: child is closed,
        // tasks are joined, and tracker returns to 0.
        e.close().await.unwrap();
        assert!(
            closed.load(Ordering::SeqCst),
            "child executor must be closed"
        );
        assert_eq!(tracker.bytes_consumed(), 0);
        drop(e);
        drop(ectx);
        drop(sctx);
        assert!(dropped.load(Ordering::SeqCst));
        assert_eq!(Arc::strong_count(&tracker), 1);
    }
}

/// Runs parallel HashAgg on a single-threaded current_thread runtime AND with an instrumented
/// 1-thread `tidb_runtime::Runtime` (`ExecPool`, spec §5.3).
#[tokio::test(flavor = "current_thread")]
async fn hash_agg_parallel_on_single_thread_runtime() {
    use tidb_runtime::{Runtime, RuntimeConfig};

    let tps = child_types();
    let sctx = parallel_ctx(4, 4);
    let aggs = [
        desc(&sctx, ast::AggFuncCount, col(&tps[1], 1), false),
        desc(&sctx, ast::AggFuncSum, col(&tps[1], 1), false),
    ];
    let rows: Vec<(i64, f64)> = (0..1000).map(|i| (i % 50, 1.0)).collect();
    let input: Vec<Chunk> = rows.chunks(32).flat_map(|c| batches(&[c])).collect();

    // A. Direct current_thread tokio runtime (pool = None).
    let ectx = exec_ctx(&sctx);
    let (mut e, _, _) = build_hash_agg(&sctx, input.clone(), None, vec![col(&tps[0], 0)], &aggs);
    e.open(&ectx).await.unwrap();
    let (out, err) = run_exec(&ectx, &mut e).await;
    assert!(err.is_none());
    assert_eq!(out.len(), 50);
    for r in &out {
        assert_eq!(r, "20,20");
    }
    e.close().await.unwrap();
    assert_eq!(e.mem_tracker.bytes_consumed(), 0);

    // B. Instrumented 1-thread ExecPool.
    let rt = Runtime::new(RuntimeConfig {
        worker_threads: 1,
        ..RuntimeConfig::default()
    })
    .unwrap();
    let mut ectx_pool = exec_ctx(&sctx);
    ectx_pool.pool = Some(rt.pool().clone());
    let (mut e2, _, _) = build_hash_agg(&sctx, input, None, vec![col(&tps[0], 0)], &aggs);
    e2.open(&ectx_pool).await.unwrap();
    let (out2, err2) = run_exec(&ectx_pool, &mut e2).await;
    assert!(err2.is_none());
    assert_eq!(out2.len(), 50);
    e2.close().await.unwrap();
    assert_eq!(e2.mem_tracker.bytes_consumed(), 0);
}

/// Converted parallel spill tests (Go `TestGetCorrectResult` & `TestFallBackAction` in
/// `agg_spill_test.go`, spec §5.1 & §5.3):
/// 1. Tight soft limit triggers `ParallelAggSpillDiskAction` -> `needSpill` ->
///    `HashAggExec.spill` returns `blocked-by:D01.10`, zero rows emitted, all workers exit,
///    tracker returns to 0.
/// 2. Pre-consumed 80% of limit -> agg tracker < 20% -> fallback action fires instead of
///    spilling.
#[tokio::test]
async fn hash_agg_parallel_spill_boundary_and_fallback() {
    use std::sync::atomic::AtomicUsize;
    use tidb_memory::action::{ActionOnExceed, ActionRef, BaseOOMAction, DefSpillPriority};
    use tidb_memory::tracker::Tracker;

    struct CountFallback {
        base: BaseOOMAction,
        hits: Arc<AtomicUsize>,
    }
    impl ActionOnExceed for CountFallback {
        fn action(&self, _t: &Arc<Tracker>) -> Result<(), Error> {
            self.hits.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn set_fallback(&self, a: Option<ActionRef>) {
            self.base.set_fallback(a)
        }
        fn get_fallback(&self) -> Option<ActionRef> {
            self.base.get_fallback()
        }
        fn get_priority(&self) -> i64 {
            0
        }
        fn set_finished(&self) {
            self.base.set_finished()
        }
        fn is_finished(&self) -> bool {
            self.base.is_finished()
        }
    }

    let tps = child_types();

    // 1. Trigger parallel spill -> blocked-by:D01.10, zero rows, workers exit, tracker == 0.
    {
        let mut sctx = parallel_ctx(4, 4);
        let sess_tracker = new_tracker(1, 32 << 10);
        let stmt_tracker = new_tracker(2, -1);
        stmt_tracker.attach_to(&sess_tracker).unwrap();
        sctx.vars.stmt_ctx_mut().mem_tracker = Some(Arc::clone(&stmt_tracker));
        let ectx = exec_ctx(&sctx);
        let aggs = [
            desc(&sctx, ast::AggFuncFirstRow, col(&tps[0], 0), false),
            desc(&sctx, ast::AggFuncSum, col(&tps[1], 1), false),
            desc(&sctx, ast::AggFuncCount, col(&tps[1], 1), false),
            desc(&sctx, ast::AggFuncAvg, col(&tps[1], 1), false),
            desc(&sctx, ast::AggFuncMin, col(&tps[1], 1), false),
            desc(&sctx, ast::AggFuncMax, col(&tps[1], 1), false),
        ];
        let rows: Vec<(i64, f64)> = (0..4000).map(|i| (i, i as f64)).collect();
        let input: Vec<Chunk> = rows.chunks(64).flat_map(|c| batches(&[c])).collect();
        let (mut e, _, closed) = build_hash_agg(&sctx, input, None, vec![col(&tps[0], 0)], &aggs);
        e.session_mem_tracker = Some(Arc::clone(&sess_tracker));
        e.open(&ectx).await.unwrap();
        assert_eq!(
            e.parallel_agg_spill_action.as_ref().unwrap().get_priority(),
            DefSpillPriority
        );
        let (out, err) = run_exec(&ectx, &mut e).await;
        assert!(out.is_empty(), "must emit zero rows before spill error");
        let err = err.expect("parallel spill must stop at D01.10 boundary");
        assert_eq!(blocked_by(&err), Some("D01.10"), "{err}");
        e.close().await.unwrap();
        assert!(closed.load(Ordering::SeqCst));
        assert_eq!(e.mem_tracker.bytes_consumed(), 0);
        assert_eq!(stmt_tracker.bytes_consumed(), 0);
        assert_eq!(sess_tracker.bytes_consumed(), 0);
    }

    // 2. Fallback action when agg tracker holds < 20% of session limit (TestFallBackAction).
    {
        let mut sctx = parallel_ctx(3, 3);
        let limit: i64 = 10 << 20; // 10 MiB
        let sess_tracker = new_tracker(1, limit);
        let fb_hits = Arc::new(AtomicUsize::new(0));
        sess_tracker.set_action_on_exceed(Some(Arc::new(CountFallback {
            base: BaseOOMAction::default(),
            hits: Arc::clone(&fb_hits),
        })));
        let stmt_tracker = new_tracker(2, -1);
        stmt_tracker.attach_to(&sess_tracker).unwrap();
        sctx.vars.stmt_ctx_mut().mem_tracker = Some(Arc::clone(&stmt_tracker));
        // Pre-consume ~99% of limit on session tracker so agg uses << 20% when limit trips.
        sess_tracker.record(limit - 1024);

        let ectx = exec_ctx(&sctx);
        let aggs = [desc(&sctx, ast::AggFuncSum, col(&tps[1], 1), false)];
        let rows: Vec<(i64, f64)> = (0..1000).map(|i| (i, i as f64)).collect();
        let input: Vec<Chunk> = rows.chunks(64).flat_map(|c| batches(&[c])).collect();
        let (mut e, _, _) = build_hash_agg(&sctx, input, None, vec![col(&tps[0], 0)], &aggs);
        e.session_mem_tracker = Some(Arc::clone(&sess_tracker));
        e.open(&ectx).await.unwrap();
        let (out, err) = run_exec(&ectx, &mut e).await;
        assert!(err.is_none(), "expected fallback (not spill): {err:?}");
        assert_eq!(out.len(), 1000);
        assert!(fb_hits.load(Ordering::SeqCst) > 0);
        e.close().await.unwrap();
        assert_eq!(e.mem_tracker.bytes_consumed(), 0);
        assert_eq!(stmt_tracker.bytes_consumed(), 0);
    }
}

/// Tests `GetGroupKey` for ENUM (`EnumSetAsIntFlag`) and DECIMAL (`flen = 0`), both
/// vectorized and non-vectorized (spec §4.9).
#[test]
fn test_get_group_key_enum_and_decimal() {
    use tidb_base::mysql::{TypeEnum, TypeNewDecimal};
    use tidb_expression::eval::RpnState;
    use tidb_expression::expression::EvalState;
    use tidb_types::MyDecimal;
    use tidb_types::enum_type::Enum;

    let sctx = ctx();
    let ectx = sctx.get_expr_ctx();
    let eval_ctx = ectx.get_eval_ctx();
    let sc = sctx.vars.stmt_ctx();
    let loc = sc.time_zone();
    let err_ctx = sc.err_ctx();

    // ENUM column: value 0 ("") and valid value 1 ("") must produce DIFFERENT group keys
    // because EnumSetAsIntFlag encodes the integer value (0 vs 1) instead of the empty name.
    let mut enum_ft = new_field_type(TypeEnum);
    enum_ft.set_elems(Some(vec![b"".to_vec(), b"a".to_vec()]));
    let mut dec_ft = new_field_type(TypeNewDecimal);
    dec_ft.set_flen(10);
    dec_ft.set_decimal(2);
    let types = vec![enum_ft.clone(), dec_ft.clone()];

    let mut chk = new_chunk_with_capacity(&types, 3);
    chk.append_enum(
        0,
        &Enum {
            name: vec![],
            value: 0,
        },
    );
    let mut d1 = MyDecimal::default();
    let _ = d1.from_string(b"12.34");
    chk.append_my_decimal(1, &d1);

    chk.append_enum(
        0,
        &Enum {
            name: vec![],
            value: 1,
        },
    );
    chk.append_my_decimal(1, &d1);

    chk.append_null(0);
    chk.append_null(1);

    let items = vec![col(&enum_ft, 0), col(&dec_ft, 1)];
    let st = EvalState::default();
    let mut rpn = RpnState::default();
    let keys_vec = super::get_group_key(
        &*eval_ctx,
        &err_ctx,
        &loc,
        true,
        &st,
        &mut rpn,
        &chk,
        Vec::new(),
        &items,
    )
    .unwrap();
    let keys_row = super::get_group_key(
        &*eval_ctx,
        &err_ctx,
        &loc,
        false,
        &st,
        &mut rpn,
        &chk,
        Vec::new(),
        &items,
    )
    .unwrap();
    assert_eq!(keys_vec, keys_row);
    assert_eq!(keys_vec.len(), 3);
    assert_ne!(keys_vec[0], keys_vec[1], "enum 0 vs enum 1 must differ");
    assert_ne!(keys_vec[0], keys_vec[2]);
}

/// Repeated mixed failure/early-close/reopen runs (converted from Go's `TestRandomFail` in
/// `agg_spill_test.go`, spec §5.1): verifies no hangs and tracker returns to 0 on every run.
#[tokio::test]
async fn hash_agg_parallel_random_fail_and_reopen_stress() {
    let tps = child_types();
    let rows: Vec<(i64, f64)> = (0..1000).map(|i| (i % 200, i as f64)).collect();
    let input: Vec<Chunk> = rows.chunks(32).flat_map(|c| batches(&[c])).collect();

    for iter in 0..10usize {
        let mut sctx = parallel_ctx(3, 3);
        sctx.vars.init_chunk_size = 16;
        sctx.vars.max_chunk_size = 16;
        let ectx = exec_ctx(&sctx);
        let aggs = [
            desc(&sctx, ast::AggFuncCount, col(&tps[1], 1), false),
            desc(&sctx, ast::AggFuncSum, col(&tps[1], 1), false),
        ];
        let err_at = if iter % 3 == 0 { Some(iter % 5) } else { None };
        let (mut e, _, closed) =
            build_hash_agg(&sctx, input.clone(), err_at, vec![col(&tps[0], 0)], &aggs);
        e.open(&ectx).await.unwrap();
        if iter % 3 == 1 {
            // Early Close after reading one chunk.
            let mut chk = exec::new_first_chunk(&e);
            let _ = e.next(&ectx, &mut chk).await;
        } else {
            let (_, err) = run_exec(&ectx, &mut e).await;
            assert_eq!(err.is_some(), err_at.is_some());
        }
        e.close().await.unwrap();
        assert!(closed.load(Ordering::SeqCst));
        assert_eq!(e.mem_tracker.bytes_consumed(), 0);
    }

    // Verify error paths of initForParallelExec (zero concurrency) and newSpillHelper
    // (mismatched agg lengths).
    let sctx = parallel_ctx(0, 0);
    let ectx = exec_ctx(&sctx);
    let aggs = [desc(&sctx, ast::AggFuncCount, col(&tps[1], 1), false)];
    let (mut e, _, _) = build_hash_agg(&sctx, Vec::new(), None, Vec::new(), &aggs);
    e.is_unparallel_exec = false;
    e.partial_concurrency = 0;
    let err = e.open(&ectx).await.unwrap_err();
    assert_eq!(
        err.to_string(),
        "partialConcurrency or finalConcurrency is 0"
    );

    let t = new_tracker(1, -1);
    let err = super::agg_spill::new_spill_helper(
        t,
        vec![None],
        Vec::new(),
        Box::new(|| new_chunk_with_capacity::<FieldType>(&[], 1)),
        Vec::new(),
    )
    .err()
    .unwrap();
    assert_eq!(
        err.to_string(),
        "len(aggFuncsForRestoring) != len(finalWorkerAggFuncs)"
    );
}

/// Covers re-Open after Close (both unparallel — including early-Close #385 regression — and
/// parallel), asymmetric concurrency (P=1/F=2 and P=2/F=1), builder non-positive concurrency
/// fallback to unparallel, final-worker nil AggFunc error propagation, and final-worker KILL
/// checkpointing.
#[tokio::test]
async fn hash_agg_reopen_asymmetric_concurrency_nil_and_kill() {
    use tidb_chunk::ColAlloc;
    use tidb_memory::TrackedAlloc;
    use tidb_memory::sqlkiller::QueryInterrupted;
    use tidb_runtime::WaitGroup;
    use tokio::sync::{mpsc, watch};

    let tps = child_types();

    // 1. Unparallel re-Open after early Close (#385): Run 1 has 3 groups {1, 2, 3}, reads only
    //    1 row then Closes; Run 2 feeds a disjoint group {99} and must output ONLY [1] (no
    //    duplicate or phantom zero rows from Run 1).
    {
        struct MultiRunSource {
            base: BaseExecutorV2,
            runs: Vec<Vec<Chunk>>,
            run: usize,
            ptr: usize,
        }
        #[async_trait]
        impl Executor for MultiRunSource {
            fn base(&self) -> &BaseExecutorV2 {
                &self.base
            }
            fn base_mut(&mut self) -> &mut BaseExecutorV2 {
                &mut self.base
            }
            async fn open(&mut self, _ctx: &ExecCtx) -> Result<(), Error> {
                self.ptr = 0;
                Ok(())
            }
            async fn next(&mut self, _ctx: &ExecCtx, req: &mut Chunk) -> Result<(), Error> {
                req.reset();
                if self.run >= self.runs.len() || self.ptr >= self.runs[self.run].len() {
                    return Ok(());
                }
                let b = &self.runs[self.run][self.ptr];
                req.append(b, 0, b.num_rows());
                self.ptr += 1;
                Ok(())
            }
            async fn close(&mut self) -> Result<(), Error> {
                self.run += 1;
                Ok(())
            }
        }

        let mut sctx = unparallel_ctx();
        sctx.vars.init_chunk_size = 1;
        sctx.vars.max_chunk_size = 1;
        let ectx = exec_ctx(&sctx);
        let aggs = [desc(&sctx, ast::AggFuncCount, col(&tps[1], 1), false)];
        let src = MultiRunSource {
            base: new_base_executor_v2(
                sctx.get_session_vars(),
                &sctx.sql_killer(),
                Some(schema_of(&tps)),
                1,
                Vec::new(),
            ),
            runs: vec![
                batches(&[&[(1, 10.0), (2, 10.0), (3, 10.0)]]),
                batches(&[&[(99, 10.0)]]),
            ],
            run: 0,
            ptr: 0,
        };
        let out_schema = schema_of(&[(*aggs[0].base.ret_tp).clone()]);
        let mut b = new_executor_builder(&sctx, None, Arc::new(TelemetryInfo::default()));
        let mut e = b
            .build_hash_agg_from_child_exec(
                &sctx,
                Some(Box::new(src)),
                &out_schema,
                2,
                &[col(&tps[0], 0)],
                &aggs,
            )
            .unwrap();
        assert!(e.is_unparallel_exec);

        e.open(&ectx).await.unwrap();
        let mut chk = exec::new_first_chunk(&e);
        e.next(&ectx, &mut chk).await.unwrap();
        assert_eq!(chk.num_rows(), 1);
        e.close().await.unwrap();
        assert_eq!(e.mem_tracker.bytes_consumed(), 0);

        e.open(&ectx).await.unwrap();
        let (out, err) = run_exec(&ectx, &mut e).await;
        assert!(err.is_none());
        assert_eq!(out, vec!["1"]);
        e.close().await.unwrap();
        assert_eq!(e.mem_tracker.bytes_consumed(), 0);
    }

    // 2. Parallel re-Open on the same HashAggExec after Close.
    {
        let sctx = parallel_ctx(3, 3);
        let ectx = exec_ctx(&sctx);
        let aggs = [
            desc(&sctx, ast::AggFuncCount, col(&tps[1], 1), false),
            desc(&sctx, ast::AggFuncSum, col(&tps[1], 1), false),
        ];
        let input = batches(&[&[(1, 2.0), (2, 3.0)], &[(1, 4.0), (3, 5.0)]]);
        let (mut e, _, closed) = build_hash_agg(&sctx, input, None, vec![col(&tps[0], 0)], &aggs);
        assert!(!e.is_unparallel_exec);

        for _ in 0..2 {
            closed.store(false, Ordering::SeqCst);
            e.open(&ectx).await.unwrap();
            let (mut out, err) = run_exec(&ectx, &mut e).await;
            assert!(err.is_none());
            out.sort();
            assert_eq!(out, vec!["1,3", "1,5", "2,6"]);
            e.close().await.unwrap();
            assert!(closed.load(Ordering::SeqCst));
            assert_eq!(e.mem_tracker.bytes_consumed(), 0);
        }
    }

    // 3. Asymmetric parallel concurrency: P=1, F=2 and P=2, F=1.
    for (p, f) in [(1, 2), (2, 1)] {
        let sctx = parallel_ctx(p, f);
        let ectx = exec_ctx(&sctx);
        let aggs = [desc(&sctx, ast::AggFuncSum, col(&tps[1], 1), false)];
        let rows: Vec<(i64, f64)> = (0..100).map(|i| (i % 10, 1.0)).collect();
        let input: Vec<Chunk> = rows.chunks(16).flat_map(|c| batches(&[c])).collect();
        let (mut e, _, _) = build_hash_agg(&sctx, input, None, vec![col(&tps[0], 0)], &aggs);
        assert!(!e.is_unparallel_exec, "({p}, {f}) should stay parallel");
        e.open(&ectx).await.unwrap();
        let (out, err) = run_exec(&ectx, &mut e).await;
        assert!(err.is_none());
        assert_eq!(out.len(), 10);
        for r in out {
            assert_eq!(r, "10");
        }
        e.close().await.unwrap();
        assert_eq!(e.mem_tracker.bytes_consumed(), 0);
    }

    // 4. Builder selects unparallel when effective concurrency <= 0 (-1 is ConcurrencyUnset
    //    and falls back to tidb_executor_concurrency, so test 0 and -2).
    for (p, f) in [(0, 4), (4, 0), (0, 0), (-2, 4), (4, -2)] {
        let mut sctx = ctx();
        sctx.vars.set_hash_agg_partial_concurrency(p);
        sctx.vars.set_hash_agg_final_concurrency(f);
        let ectx = exec_ctx(&sctx);
        let aggs = [desc(&sctx, ast::AggFuncCount, col(&tps[1], 1), false)];
        let (mut e, _, _) = build_hash_agg(
            &sctx,
            batches(&[&[(1, 1.0), (1, 2.0)]]),
            None,
            vec![col(&tps[0], 0)],
            &aggs,
        );
        assert!(
            e.is_unparallel_exec,
            "builder must pick unparallel for concurrency ({p}, {f})"
        );
        e.open(&ectx).await.unwrap();
        let (out, err) = run_exec(&ectx, &mut e).await;
        assert!(err.is_none());
        assert_eq!(out, vec!["2"]);
        e.close().await.unwrap();
    }

    // 5. Final worker with nil AggFunc returns nil-pointer error (NOT swallowed by #384 path).
    {
        let sctx = parallel_ctx(2, 2);
        let ectx = exec_ctx(&sctx);
        let aggs = [desc(&sctx, ast::AggFuncCount, col(&tps[1], 1), false)];
        let (mut e, _, _) = build_hash_agg(
            &sctx,
            batches(&[&[(1, 1.0)]]),
            None,
            vec![col(&tps[0], 0)],
            &aggs,
        );
        e.final_agg_funcs = vec![None];
        e.open(&ectx).await.unwrap();
        let (_, err) = run_exec(&ectx, &mut e).await;
        let err = err.expect("nil final AggFunc must return error, not be swallowed");
        assert!(
            err.to_string()
                .contains("runtime error: invalid memory address or nil pointer dereference"),
            "unexpected error: {err}"
        );
        e.close().await.unwrap();
        assert_eq!(e.mem_tracker.bytes_consumed(), 0);
    }

    // 6. Final worker CheckpointStride KILL hook returns ErrQueryInterrupted inside merge/output.
    {
        let sctx = parallel_ctx(2, 2);
        let killer = sctx.sql_killer();
        let base_killer = new_base_executor_v2(
            sctx.get_session_vars(),
            &killer,
            Some(schema_of(&tps)),
            99,
            Vec::new(),
        )
        .killer;
        let tracker = new_tracker(99, -1);
        let alloc = TrackedAlloc::new(Arc::clone(&tracker));
        let (_finish_tx, finish_rx) = watch::channel(false);
        let (input_tx, input_rx) = mpsc::channel(2);
        let (output_tx, mut output_rx) = mpsc::channel(2);
        let (holder_tx, holder_rx) = mpsc::channel(1);
        holder_tx
            .try_send(new_chunk_with_capacity::<FieldType>(&[], 8))
            .unwrap();
        let spill_helper = Arc::new(
            super::agg_spill::new_spill_helper(
                Arc::clone(&tracker),
                Vec::new(),
                Vec::new(),
                Box::new(|| new_chunk_with_capacity::<FieldType>(&[], 8)),
                Vec::new(),
            )
            .unwrap(),
        );
        // Send two partial maps so mergeInputIntoResultMap enters the merge loop, and arm KILL.
        let mut m1 =
            crate::aggfuncs::new_agg_partial_result_mapper_in(ColAlloc::Tracked(alloc.clone()), 0);
        let mut m2 =
            crate::aggfuncs::new_agg_partial_result_mapper_in(ColAlloc::Tracked(alloc.clone()), 0);
        let k1 =
            super::agg_hash_executor::tracked_key_box(&[1u8], ColAlloc::Tracked(alloc.clone()));
        let k2 =
            super::agg_hash_executor::tracked_key_box(&[2u8], ColAlloc::Tracked(alloc.clone()));
        m1.set(
            k1,
            allocator_api2::vec::Vec::new_in(ColAlloc::Tracked(alloc.clone())),
        );
        m2.set(
            k2,
            allocator_api2::vec::Vec::new_in(ColAlloc::Tracked(alloc.clone())),
        );
        input_tx.send(m1).await.unwrap();
        input_tx.send(m2).await.unwrap();
        drop(input_tx);

        killer.send_kill_signal(QueryInterrupted);

        let worker = super::agg_hash_final_worker::HashAggFinalWorker {
            base: super::agg_hash_base_worker::new_base_hash_agg_worker(
                finish_rx,
                Vec::new(),
                8,
                Arc::clone(&tracker),
                alloc,
            ),
            partial_result_map: crate::aggfuncs::new_agg_partial_result_mapper_in(
                ColAlloc::Global,
                0,
            ),
            input_rx,
            output_tx,
            final_result_holder_rx: holder_rx,
            final_result_holder_tx: holder_tx,
            spill_helper,
            eval_ctx: sctx.get_expr_ctx().get_eval_ctx(),
            sql_killer: base_killer,
        };
        let pwg = Arc::new(WaitGroup::new());
        let fwg = Arc::new(WaitGroup::new());
        fwg.add(1);
        worker.run(fwg, pwg).await;

        let msg = output_rx
            .recv()
            .await
            .expect("final worker must report KILL error");
        let err = msg.err.expect("expected KILL error");
        assert!(
            err.to_string().contains("Query execution was interrupted"),
            "unexpected KILL error: {err}"
        );
    }
}

/// Ruling R21 (#384, owner 2026-11-25): parallel `HashAggExec` propagates
/// `AppendFinalResult2Chunk` errors (e.g., high-precision `DECIMAL` `AVG` / `SUM` overflow or
/// truncation) instead of Go v8.5.8's log-and-continue, which misaligns chunk columns or panics.
#[tokio::test]
async fn hash_agg_parallel_propagates_decimal_append_final_error() {
    use tidb_base::mysql::TypeNewDecimal;
    use tidb_types::MyDecimal;

    let mut dec65_30 = new_field_type(TypeNewDecimal);
    dec65_30.set_flen(65);
    dec65_30.set_decimal(30);
    let mut dec10_0 = new_field_type(TypeNewDecimal);
    dec10_0.set_flen(10);
    dec10_0.set_decimal(0);
    let int_ft = new_field_type(TypeLonglong);
    let src_types = vec![int_ft.clone(), dec65_30.clone(), dec10_0.clone()];

    // Build a 65-digit integer decimal: rounding it to frac=30 (65 + 30 > 65) or dividing in
    // AVG overflows/truncates in AppendFinalResult2Chunk.
    let mut huge = MyDecimal::default();
    huge.from_string(b"99999999999999999999999999999999999999999999999999999999999999999")
        .unwrap();
    let mut one = MyDecimal::default();
    one.from_string(b"1").unwrap();

    for agg_name in [ast::AggFuncAvg, ast::AggFuncSum] {
        let sctx = parallel_ctx(2, 2);
        let ectx = exec_ctx(&sctx);

        let mut c = new_chunk_with_capacity(&src_types, 3);
        // Group 1: huge decimal triggers AppendFinalResult2Chunk error.
        c.append_int64(0, 1);
        c.append_my_decimal(1, &huge);
        c.append_my_decimal(2, &one);
        // Group 2: ordinary decimal (in Go v8.5.8 this causes cross-group column misalignment).
        c.append_int64(0, 2);
        c.append_my_decimal(1, &one);
        c.append_my_decimal(2, &one);

        let src = BatchSource {
            base: new_base_executor_v2(
                sctx.get_session_vars(),
                &sctx.sql_killer(),
                Some(schema_of(&src_types)),
                1,
                Vec::new(),
            ),
            batches: vec![c],
            ptr: 0,
            err_at: None,
            opened: Arc::new(AtomicBool::new(false)),
            closed: Arc::new(AtomicBool::new(false)),
            dropped: None,
        };
        let aggs = [
            desc(&sctx, agg_name, col(&dec65_30, 1), false),
            desc(&sctx, ast::AggFuncCount, col(&dec10_0, 2), false),
        ];
        let out_types: Vec<FieldType> = aggs.iter().map(|d| (*d.base.ret_tp).clone()).collect();
        let mut b = new_executor_builder(&sctx, None, Arc::new(TelemetryInfo::default()));
        let mut e = b
            .build_hash_agg_from_child_exec(
                &sctx,
                Some(Box::new(src)),
                &schema_of(&out_types),
                2,
                &[col(&int_ft, 0)],
                &aggs,
            )
            .unwrap();
        assert!(!e.is_unparallel_exec);

        e.open(&ectx).await.unwrap();
        let (_, err) = run_exec(&ectx, &mut e).await;
        let err = err.unwrap_or_else(|| {
            panic!("parallel HashAgg {agg_name}(DECIMAL) overflow must return error (R21 / #384)")
        });
        assert!(
            err.to_string().contains("Data truncated") || err.to_string().contains("out of range"),
            "unexpected error for {agg_name}: {err}"
        );
        e.close().await.unwrap();
        assert_eq!(e.mem_tracker.bytes_consumed(), 0);
    }
}

/// Concurrent error sources in parallel `HashAggExec`: a partial worker hits a data error
/// (`SUM(DECIMAL)` overflow) while the fetcher hits a child `Next` error (`err_at`). Either
/// error may win the race to `finalOutputCh`; verify that one of the two is returned, all
/// workers terminate cleanly on `Close`, and the memory tracker drops back to zero with no
/// leaked `Arc<Tracker>` / `TrackedAlloc` handles.
#[tokio::test]
async fn hash_agg_parallel_concurrent_data_and_child_error() {
    use tidb_base::mysql::TypeNewDecimal;
    use tidb_types::MyDecimal;

    let mut dec65_0 = new_field_type(TypeNewDecimal);
    dec65_0.set_flen(65);
    dec65_0.set_decimal(0);
    let int_ft = new_field_type(TypeLonglong);
    let src_types = vec![int_ft.clone(), dec65_0.clone()];

    // 81 nines (max MyDecimal word buffer); adding two of them overflows DecimalAdd in the
    // partial worker's UpdatePartialResult with ErrOverflow ("DECIMAL value is out of range").
    let mut huge = MyDecimal::default();
    huge.from_string(&[b'9'; 81]).unwrap();
    let mut one = MyDecimal::default();
    one.from_string(b"1").unwrap();

    // First confirm batch 0 alone reliably produces the data overflow error (with err_at = None).
    {
        let sctx = parallel_ctx(2, 2);
        let ectx = exec_ctx(&sctx);
        let mut c = new_chunk_with_capacity(&src_types, 2);
        c.append_int64(0, 1);
        c.append_my_decimal(1, &huge);
        c.append_int64(0, 1);
        c.append_my_decimal(1, &huge);
        let src = BatchSource {
            base: new_base_executor_v2(
                sctx.get_session_vars(),
                &sctx.sql_killer(),
                Some(schema_of(&src_types)),
                1,
                Vec::new(),
            ),
            batches: vec![c],
            ptr: 0,
            err_at: None,
            opened: Arc::new(AtomicBool::new(false)),
            closed: Arc::new(AtomicBool::new(false)),
            dropped: None,
        };
        let aggs = [desc(&sctx, ast::AggFuncSum, col(&dec65_0, 1), false)];
        let out_types: Vec<FieldType> = aggs.iter().map(|d| (*d.base.ret_tp).clone()).collect();
        let mut b = new_executor_builder(&sctx, None, Arc::new(TelemetryInfo::default()));
        let mut e = b
            .build_hash_agg_from_child_exec(
                &sctx,
                Some(Box::new(src)),
                &schema_of(&out_types),
                2,
                &[col(&int_ft, 0)],
                &aggs,
            )
            .unwrap();
        e.open(&ectx).await.unwrap();
        let (_, err) = run_exec(&ectx, &mut e).await;
        let err = err.expect("huge + huge DECIMAL sum must overflow");
        assert!(
            err.to_string().contains("out of range"),
            "expected DECIMAL overflow, got: {err}"
        );
        e.close().await.unwrap();
        assert_eq!(e.mem_tracker.bytes_consumed(), 0);
    }

    // Now race the data overflow in batch 0 against mock child errors in subsequent batches.
    for round in 0..8 {
        let sctx = parallel_ctx(4, 4);
        let ectx = exec_ctx(&sctx);

        let mut input = Vec::with_capacity(16);
        for b in 0..16 {
            let mut c = new_chunk_with_capacity(&src_types, 2);
            if b == 0 {
                c.append_int64(0, 1);
                c.append_my_decimal(1, &huge);
                c.append_int64(0, 1);
                c.append_my_decimal(1, &huge);
            } else {
                c.append_int64(0, (b % 4) as i64);
                c.append_my_decimal(1, &one);
            }
            input.push(c);
        }

        let closed = Arc::new(AtomicBool::new(false));
        let dropped = Arc::new(AtomicBool::new(false));
        let src = BatchSource {
            base: new_base_executor_v2(
                sctx.get_session_vars(),
                &sctx.sql_killer(),
                Some(schema_of(&src_types)),
                1,
                Vec::new(),
            ),
            batches: input,
            ptr: 0,
            err_at: Some(1 + (round % 4)),
            opened: Arc::new(AtomicBool::new(false)),
            closed: Arc::clone(&closed),
            dropped: Some(Arc::clone(&dropped)),
        };
        let aggs = [desc(&sctx, ast::AggFuncSum, col(&dec65_0, 1), false)];
        let out_types: Vec<FieldType> = aggs.iter().map(|d| (*d.base.ret_tp).clone()).collect();
        let mut b = new_executor_builder(&sctx, None, Arc::new(TelemetryInfo::default()));
        let mut e = b
            .build_hash_agg_from_child_exec(
                &sctx,
                Some(Box::new(src)),
                &schema_of(&out_types),
                2,
                &[col(&int_ft, 0)],
                &aggs,
            )
            .unwrap();
        assert!(!e.is_unparallel_exec);
        let tracker = Arc::clone(&e.mem_tracker);

        e.open(&ectx).await.unwrap();
        let (_, err) = run_exec(&ectx, &mut e).await;
        let err = err.expect("concurrent data + child errors must fail execution");
        let msg = err.to_string();
        assert!(
            msg.contains("out of range") || msg.contains("mock child error"),
            "round {round}: expected DECIMAL overflow or child error, got: {msg}"
        );
        e.close().await.unwrap();
        assert!(closed.load(Ordering::SeqCst), "child must be closed");
        assert_eq!(tracker.bytes_consumed(), 0);
        drop(e);
        drop(ectx);
        drop(sctx);
        assert!(dropped.load(Ordering::SeqCst), "child must be dropped");
        // Strong count == 1 proves every worker exited and released all TrackedAlloc / Tracker refs.
        assert_eq!(Arc::strong_count(&tracker), 1);
    }
}

/// Regression test: consuming an unallocated `MemAwareMap<..., ColAlloc>` via `into_iter()`
/// (fixed in hashbrown 0.17.2) and merging an empty `AggPartialResultMapper<ColAlloc>` into a
/// non-empty final worker both drop `TrackedAlloc` / `Arc<Tracker>` with no leaked handles.
#[tokio::test]
async fn hash_agg_final_worker_empty_map_merge_drops_tracked_alloc() {
    use tidb_chunk::ColAlloc;
    use tidb_memory::TrackedAlloc;

    // Directly pin hashbrown >= 0.17.2: into_iter() on a zero-capacity map must drop ColAlloc.
    {
        let tr = new_tracker(1, -1);
        let alloc = TrackedAlloc::new(Arc::clone(&tr));
        let empty =
            crate::aggfuncs::new_agg_partial_result_mapper_in(ColAlloc::Tracked(alloc.clone()), 0);
        for _ in empty.m {}
        drop(alloc);
        assert_eq!(Arc::strong_count(&tr), 1);
    }

    let tps = child_types();
    let sctx = parallel_ctx(4, 4);
    let ectx = exec_ctx(&sctx);
    let aggs = [desc(&sctx, ast::AggFuncCount, col(&tps[1], 1), false)];
    // Two distinct keys in separate batches across 4 partial/final workers ensure at least one
    // final worker receives a non-empty map followed by an empty map from another partial worker.
    let input = vec![
        batches(&[&[(1, 1.0)]]).pop().unwrap(),
        batches(&[&[(3, 2.0)]]).pop().unwrap(),
    ];
    let (mut e, _, closed) = build_hash_agg(&sctx, input, None, vec![col(&tps[0], 0)], &aggs);
    assert!(!e.is_unparallel_exec);
    let tracker = Arc::clone(&e.mem_tracker);

    e.open(&ectx).await.unwrap();
    let (mut out, err) = run_exec(&ectx, &mut e).await;
    assert!(err.is_none(), "unexpected error: {err:?}");
    out.sort();
    assert_eq!(out, vec!["1", "1"]);
    e.close().await.unwrap();
    assert!(closed.load(Ordering::SeqCst));
    assert_eq!(tracker.bytes_consumed(), 0);
    drop(e);
    drop(ectx);
    drop(sctx);
    assert_eq!(Arc::strong_count(&tracker), 1);
}
