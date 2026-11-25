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
/// `err_at` and records whether it was opened and closed.
struct BatchSource {
    base: BaseExecutorV2,
    batches: Vec<Chunk>,
    ptr: usize,
    err_at: Option<usize>,
    opened: Arc<AtomicBool>,
    closed: Arc<AtomicBool>,
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
    // An aggregate function of a later part of D12.4 (GROUP_CONCAT): `b.err`, no executor.
    let gc = desc(&sctx, ast::AggFuncGroupConcat, col(&tps[1], 1), false);
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
