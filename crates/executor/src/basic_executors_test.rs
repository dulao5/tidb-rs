//! Tests of the basic executors (D12.2) beyond Go's: the parallel projection's tasks (order,
//! close, a panicking or failing child, a statement dropped without Close), the table dual,
//! the limit's inline projection, `markChildrenUsedCols` and the builder's branches.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use tidb_base::Error;
use tidb_base::mysql::TypeLonglong;
use tidb_chunk::chunk::Chunk;
use tidb_chunk::chunk_util::new_column_swap_helper;
use tidb_expression::schema::new_schema;
use tidb_expression::{Column, Schema};
use tidb_planner::PhysicalPlan;
use tidb_planner::core::physical_plans::{
    PhysicalLimit, PhysicalProjection, PhysicalSelection, PhysicalTableDual,
};
use tidb_planner::property::stats_info::StatsInfo;
use tidb_types::field_type::new_field_type;

use crate::adapter::TelemetryInfo;
use crate::adapter::tests::MockSession;
use crate::builder::{mark_children_used_cols, new_executor_builder};
use crate::exec::ExecCtx;
use crate::executor_required_rows_test::{
    build_projection_exec, column_exprs, default_ctx, eq_filter, exec_ctx,
    new_required_rows_data_source,
};
use crate::glue::{Plan, SessionContext};
use crate::internal::exec::executor::{
    self as exec, BaseExecutorV2, Executor, new_base_executor, new_base_executor_v2,
};
use crate::select::{LimitExec, TableDualExec};
use crate::testutil::mock_plan;

/// What the test source did.
#[derive(Default)]
struct Probe {
    nexts: AtomicUsize,
    closed: AtomicUsize,
    dropped: AtomicBool,
}

/// How the test source misbehaves at its `n`-th `Next` (counted from 1).
#[derive(Clone, Copy)]
enum Fault {
    None,
    Panic(usize),
    Error(usize),
    /// Never returns (a child waiting for the store).
    Hang(usize),
}

/// Produces `0..n` in one Longlong column, the parent's required rows per `Next`.
struct Source {
    base: BaseExecutorV2,
    next: i64,
    n: i64,
    fault: Fault,
    probe: Arc<Probe>,
}

fn long_schema(cols: usize) -> Schema {
    new_schema(
        (0..cols)
            .map(|i| {
                Arc::new(Column {
                    index: i as isize,
                    ret_type: Arc::new(new_field_type(TypeLonglong)),
                    ..Column::default()
                })
            })
            .collect(),
    )
}

fn source(sctx: &MockSession, n: i64, fault: Fault) -> (Box<dyn Executor>, Arc<Probe>) {
    let probe = Arc::new(Probe::default());
    let s = Source {
        base: new_base_executor_v2(
            sctx.get_session_vars(),
            &sctx.sql_killer(),
            Some(long_schema(1)),
            0,
            Vec::new(),
        ),
        next: 0,
        n,
        fault,
        probe: Arc::clone(&probe),
    };
    (Box::new(s), probe)
}

#[async_trait]
impl Executor for Source {
    fn base(&self) -> &BaseExecutorV2 {
        &self.base
    }
    fn base_mut(&mut self) -> &mut BaseExecutorV2 {
        &mut self.base
    }
    async fn next(&mut self, _ctx: &ExecCtx, req: &mut Chunk) -> Result<(), Error> {
        let call = self.probe.nexts.fetch_add(1, Ordering::SeqCst) + 1;
        match self.fault {
            Fault::Panic(at) if at == call => panic!("mock source panicked"),
            Fault::Error(at) if at == call => {
                return Err(Error::plain("mock source failed".to_owned()));
            }
            Fault::Hang(at) if at == call => std::future::pending::<()>().await,
            _ => {}
        }
        req.reset();
        while self.next < self.n && req.num_rows() < req.required_rows() {
            req.append_int64(0, self.next);
            self.next += 1;
        }
        Ok(())
    }
    async fn close(&mut self) -> Result<(), Error> {
        self.probe.closed.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        self.probe.dropped.store(true, Ordering::SeqCst);
    }
}

/// Reads the executor to its end: the values of column 0.
async fn drain(e: &mut dyn Executor, ctx: &ExecCtx) -> Result<Vec<i64>, Error> {
    let mut out = Vec::new();
    let mut chk = exec::new_first_chunk(e);
    loop {
        exec::next(ctx, e, &mut chk).await?;
        if chk.num_rows() == 0 {
            return Ok(out);
        }
        for i in 0..chk.num_rows() {
            out.push(chk.get_row(i).get_int64(0));
        }
    }
}

fn projection_over(
    sctx: &MockSession,
    child: Box<dyn Executor>,
    workers: i64,
) -> Box<dyn Executor> {
    let exprs = column_exprs(&child.schema());
    build_projection_exec(sctx, exprs, child, workers)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_projection_keeps_the_child_order_and_releases_memory() {
    let sctx = default_ctx();
    let ctx = exec_ctx(&sctx);
    let (child, probe) = source(&sctx, 10_000, Fault::None);
    let mut e = projection_over(&sctx, child, 3);
    e.open(&ctx).await.unwrap();
    let rows = drain(e.as_mut(), &ctx).await.unwrap();
    assert_eq!(rows, (0..10_000).collect::<Vec<_>>());
    e.close().await.unwrap();
    // The fetcher gave the child back: the base executor closed it.
    assert_eq!(probe.closed.load(Ordering::SeqCst), 1);
    let stmt_tracker = sctx.vars.stmt_ctx().mem_tracker.clone().unwrap();
    assert_eq!(stmt_tracker.bytes_consumed(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_projection_closed_before_the_end() {
    let sctx = default_ctx();
    let ctx = exec_ctx(&sctx);
    let (child, probe) = source(&sctx, 100_000, Fault::None);
    let mut e = projection_over(&sctx, child, 4);
    e.open(&ctx).await.unwrap();
    let mut chk = exec::new_first_chunk(e.as_ref());
    for _ in 0..3 {
        exec::next(&ctx, e.as_mut(), &mut chk).await.unwrap();
        assert!(chk.num_rows() > 0);
    }
    e.close().await.unwrap();
    assert_eq!(probe.closed.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_projection_child_panic_is_an_error() {
    let sctx = default_ctx();
    let ctx = exec_ctx(&sctx);
    let (child, probe) = source(&sctx, 100_000, Fault::Panic(2));
    let mut e = projection_over(&sctx, child, 2);
    e.open(&ctx).await.unwrap();
    let err = drain(e.as_mut(), &ctx).await.unwrap_err();
    assert!(err.to_string().contains("mock source panicked"), "{err}");
    e.close().await.unwrap();
    // The child survived the panic inside the fetcher and was closed.
    assert_eq!(probe.closed.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_projection_child_error() {
    let sctx = default_ctx();
    let ctx = exec_ctx(&sctx);
    let (child, probe) = source(&sctx, 100_000, Fault::Error(1));
    let mut e = projection_over(&sctx, child, 2);
    e.open(&ctx).await.unwrap();
    let err = drain(e.as_mut(), &ctx).await.unwrap_err();
    assert_eq!(err.to_string(), "mock source failed");
    e.close().await.unwrap();
    assert_eq!(probe.closed.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_projection_dropped_without_close_stops_its_tasks() {
    let sctx = default_ctx();
    let ctx = exec_ctx(&sctx);
    let (child, probe) = source(&sctx, 100_000, Fault::Hang(2));
    let mut e = projection_over(&sctx, child, 2);
    e.open(&ctx).await.unwrap();
    let mut chk = exec::new_first_chunk(e.as_ref());
    exec::next(&ctx, e.as_mut(), &mut chk).await.unwrap();
    // The fetcher now waits for the child forever; a cancelled statement drops the tree.
    drop(e);
    for _ in 0..100 {
        if probe.dropped.load(Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        probe.dropped.load(Ordering::SeqCst),
        "the fetcher task kept the child"
    );
    assert_eq!(probe.closed.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn unparallel_projection_without_vectorizable_exprs() {
    // A projection asked for workers falls back to one task when its expressions cannot be
    // vectorized; here they can, so the parallel path runs on a current-thread runtime too.
    let sctx = default_ctx();
    let ctx = exec_ctx(&sctx);
    let (child, _) = source(&sctx, 3000, Fault::None);
    let mut e = projection_over(&sctx, child, 2);
    e.open(&ctx).await.unwrap();
    assert_eq!(drain(e.as_mut(), &ctx).await.unwrap().len(), 3000);
    e.close().await.unwrap();
}

#[tokio::test]
async fn selection_filters_rows() {
    let sctx = default_ctx();
    let ctx = exec_ctx(&sctx);
    let (child, probe) = source(&sctx, 10, Fault::None);
    let col = Arc::clone(&child.schema().columns[0]);
    let mut e = crate::executor_required_rows_test::build_selection_exec(
        &sctx,
        vec![eq_filter(&sctx, &col, 7)],
        child,
    );
    e.open(&ctx).await.unwrap();
    assert_eq!(drain(e.as_mut(), &ctx).await.unwrap(), vec![7]);
    e.close().await.unwrap();
    assert_eq!(probe.closed.load(Ordering::SeqCst), 1);
}

fn table_dual(sctx: &MockSession, rows: isize, cols: usize) -> Box<dyn Executor> {
    let mut base = new_base_executor_v2(
        sctx.get_session_vars(),
        &sctx.sql_killer(),
        Some(long_schema(cols)),
        0,
        Vec::new(),
    );
    base.alloc.set_init_cap(rows);
    Box::new(TableDualExec {
        base,
        num_dual_rows: rows,
        num_returned: 0,
    })
}

#[tokio::test]
async fn table_dual_rows() {
    let sctx = default_ctx();
    let ctx = exec_ctx(&sctx);

    let mut e = table_dual(&sctx, 1, 2);
    e.open(&ctx).await.unwrap();
    let mut chk = exec::new_first_chunk(e.as_ref());
    e.next(&ctx, &mut chk).await.unwrap();
    assert_eq!(chk.num_rows(), 1);
    assert!(chk.get_row(0).is_null(0) && chk.get_row(0).is_null(1));
    e.next(&ctx, &mut chk).await.unwrap();
    assert_eq!(chk.num_rows(), 0);
    // Open again: one row again.
    e.open(&ctx).await.unwrap();
    e.next(&ctx, &mut chk).await.unwrap();
    assert_eq!(chk.num_rows(), 1);

    let mut e = table_dual(&sctx, 0, 1);
    e.open(&ctx).await.unwrap();
    e.next(&ctx, &mut chk).await.unwrap();
    assert_eq!(chk.num_rows(), 0);

    // No column: one virtual row (`SELECT 1` without a FROM, after column pruning).
    let mut e = table_dual(&sctx, 1, 0);
    e.open(&ctx).await.unwrap();
    let mut chk = exec::new_first_chunk(e.as_ref());
    e.next(&ctx, &mut chk).await.unwrap();
    assert_eq!(chk.num_rows(), 1);
}

/// The limit keeps column 1 of its child only (an inline projection): the first batch goes
/// through `Prune`, the later ones through the column swap helper.
#[tokio::test]
async fn limit_inline_projection() {
    let sctx = default_ctx();
    let ctx = exec_ctx(&sctx);
    let (ds, _) = new_required_rows_data_source(&sctx, 100, None);
    let mut out_col = Column::clone(&ds.base.meta.schema().columns[1]);
    out_col.index = 1;
    let mut base = new_base_executor(
        sctx.get_session_vars(),
        &sctx.sql_killer(),
        Some(new_schema(vec![Arc::new(out_col)])),
        0,
        vec![Box::new(ds)],
    );
    base.base.alloc.set_init_cap(10);
    let mut e: Box<dyn Executor> = Box::new(LimitExec {
        base,
        begin: 3,
        end: 13,
        cursor: 0,
        meet_first_batch: false,
        child_result: None,
        column_idxs_used_by_child: Some(vec![1]),
        column_swap_helper: Some(new_column_swap_helper(&[1])),
    });
    e.open(&ctx).await.unwrap();
    let mut chk = exec::new_first_chunk(e.as_ref());
    let mut got = Vec::new();
    loop {
        chk.set_required_rows(4, 1024);
        e.next(&ctx, &mut chk).await.unwrap();
        if chk.num_rows() == 0 {
            break;
        }
        assert_eq!(chk.num_cols(), 1);
        for i in 0..chk.num_rows() {
            got.push(chk.get_row(i).get_int64(0));
        }
    }
    // The default generator gives row r the values 2r+1 (double) and 2r+2 (longlong).
    assert_eq!(got, (3..13).map(|r| 2 * r + 2).collect::<Vec<_>>());
    e.close().await.unwrap();
}

/// The example in Go's comment of markChildrenUsedCols.
#[test]
fn mark_children_used_cols_keeps_the_output_order() {
    let col = |index: isize| {
        Arc::new(Column {
            index,
            ..Column::default()
        })
    };
    // left child schema: [col11]; right child schema: [col21, col22];
    // output schema is [col11, col22, col21].
    let output = vec![col(0), col(2), col(1)];
    let left = new_schema(vec![col(0)]);
    let right = new_schema(vec![col(0), col(1)]);
    assert_eq!(
        mark_children_used_cols(&output, &[&left, &right]),
        vec![vec![0], vec![1, 0]]
    );
    assert_eq!(
        mark_children_used_cols(&[col(1)], &[&new_schema(vec![col(0), col(1), col(2)])]),
        vec![vec![1]]
    );
}

/// Builds `p` and reads it to the end.
async fn build_and_drain(sctx: &mut MockSession, p: &Plan) -> Result<Vec<i64>, Error> {
    let mut b = new_executor_builder(&*sctx, None, Arc::new(TelemetryInfo::default()));
    let Some(mut e) = b.build(sctx, Some(p)).await else {
        return Err(b.err.expect("no executor without an error"));
    };
    let ctx = exec_ctx(sctx);
    e.open(&ctx).await?;
    let rows = drain(e.as_mut(), &ctx).await;
    e.close().await?;
    rows
}

#[tokio::test]
async fn builder_builds_the_basic_executors() {
    let mut sctx = default_ctx();

    let (child, counters) = mock_plan(10);
    let mut limit = PhysicalLimit {
        offset: 2,
        count: 3,
        ..PhysicalLimit::default()
    };
    limit.set_schema(child.schema().clone());
    limit.sp.base.children = vec![child];
    let limit: Plan = Arc::new(limit);
    assert_eq!(
        build_and_drain(&mut sctx, &limit).await.unwrap(),
        vec![2, 3, 4]
    );
    assert_eq!(counters.closed.load(Ordering::SeqCst), 1);

    let (child, _) = mock_plan(4);
    let mut proj = PhysicalProjection {
        exprs: column_exprs(child.schema()),
        ..PhysicalProjection::default()
    };
    proj.set_schema(child.schema().clone());
    proj.sp.base.children = vec![child];
    proj.sp.base.plan.set_stats(Some(stats(4.0)));
    let proj: Plan = Arc::new(proj);
    assert_eq!(
        build_and_drain(&mut sctx, &proj).await.unwrap(),
        vec![0, 1, 2, 3]
    );

    let (child, _) = mock_plan(4);
    let mut sel = PhysicalSelection::default();
    sel.base.children = vec![child];
    let sel: Plan = Arc::new(sel);
    assert_eq!(
        build_and_drain(&mut sctx, &sel).await.unwrap(),
        vec![0, 1, 2, 3]
    );

    let mut dual = PhysicalTableDual {
        row_count: 1,
        ..PhysicalTableDual::default()
    };
    dual.set_schema(long_schema(1));
    let dual: Plan = Arc::new(dual);
    let mut b = new_executor_builder(&sctx, None, Arc::new(TelemetryInfo::default()));
    assert!(b.build(&mut sctx, Some(&dual)).await.is_some());

    let mut dual = PhysicalTableDual {
        row_count: 2,
        ..PhysicalTableDual::default()
    };
    dual.set_schema(long_schema(1));
    let dual: Plan = Arc::new(dual);
    let err = build_and_drain(&mut sctx, &dual).await.unwrap_err();
    assert_eq!(
        err.to_string(),
        "buildTableDual failed, invalid row count for dual table: 2"
    );
}

/// The plan's estimated rows.
fn stats(row_count: f64) -> Arc<StatsInfo> {
    Arc::new(StatsInfo {
        row_count,
        ..StatsInfo::default()
    })
}

/// The projection's builder runs it with `tidb_projection_concurrency` workers only when the
/// plan expects at least a chunk of rows and the statement does not lock or write.
#[tokio::test]
async fn builder_projection_concurrency() {
    let mut sctx = default_ctx();
    let projection = |row_count: f64| -> Plan {
        let (child, _) = mock_plan(1);
        let mut proj = PhysicalProjection {
            exprs: column_exprs(child.schema()),
            ..PhysicalProjection::default()
        };
        proj.set_schema(child.schema().clone());
        proj.sp.base.children = vec![child];
        proj.sp.base.plan.set_stats(Some(stats(row_count)));
        Arc::new(proj)
    };
    let concurrency = sctx.vars.projection_concurrency();
    assert!(concurrency > 0);
    for (row_count, has_lock, want) in [
        (1023.0, false, 0),
        (1024.0, false, concurrency),
        (1e6, true, 0),
    ] {
        let mut b = new_executor_builder(&sctx, None, Arc::new(TelemetryInfo::default()));
        b.has_lock = has_lock;
        let e = b
            .build(&mut sctx, Some(&projection(row_count)))
            .await
            .unwrap();
        assert_eq!(e.as_projection().unwrap().num_workers, want, "{row_count}");
    }
}

/// A filter that is not vectorizable (Go: a user variable assignment, `SETVAR`) runs the
/// selection row by row: each `Next` returns as soon as one input row is selected.
#[tokio::test]
async fn selection_un_batched_next_returns_one_row_per_next() {
    use crate::select::{SelectionExec, new_selection_executor_context};
    use tidb_expression::eval::RpnState;
    use tidb_expression::expression::{EvalState, Expr};
    use tidb_expression::{Constant, ExprRef};
    use tidb_types::datum::new_int_datum;

    let sctx = default_ctx();
    let ctx = exec_ctx(&sctx);
    let (src, probe) = source(&sctx, 10, Fault::None);
    let col: ExprRef = Expr::Column(Column::clone(&src.schema().columns[0])).into_ref();
    let ge = tidb_expression::scalar_function::new_function(
        &*sctx.get_expr_ctx(),
        "ge",
        new_field_type(TypeLonglong),
        vec![
            col,
            Expr::Constant(Constant::new(
                new_int_datum(5),
                new_field_type(TypeLonglong),
            ))
            .into_ref(),
        ],
    )
    .unwrap();
    let mut sel = SelectionExec {
        ctx: new_selection_executor_context(&sctx),
        base: new_base_executor_v2(
            sctx.get_session_vars(),
            &sctx.sql_killer(),
            Some(src.schema()),
            0,
            vec![src],
        ),
        batched: false,
        filters: vec![ge],
        selected: Vec::new(),
        input_row: None,
        child_result: None,
        mem_tracker: None,
        eval_state: EvalState::new(),
        rpn: RpnState::default(),
    };
    sel.open(&ctx).await.unwrap();
    assert!(sel.batched, "a comparison is vectorizable");
    // What open would set for a filter that is not.
    sel.batched = false;
    let mut chk = exec::new_first_chunk(&sel);
    let mut got = Vec::new();
    loop {
        sel.next(&ctx, &mut chk).await.unwrap();
        if chk.num_rows() == 0 {
            break;
        }
        assert_eq!(chk.num_rows(), 1, "one selected row per Next");
        got.push(chk.get_row(0).get_int64(0));
    }
    assert_eq!(got, vec![5, 6, 7, 8, 9]);
    sel.close().await.unwrap();
    assert_eq!(probe.closed.load(Ordering::SeqCst), 1);
}

/// `checkMaxExecutionTimeExceeded`: only a SELECT with `max_execution_time` set and a
/// process start time can exceed it, once the elapsed time reaches the limit.
#[test]
fn check_max_execution_time_exceeded_cases() {
    use crate::select::check_max_execution_time_exceeded as check;
    use std::time::SystemTime;

    let mut sctx = default_ctx();
    let long_ago = SystemTime::now() - Duration::from_secs(10);
    assert!(check(None, Some(long_ago)).is_ok(), "no session");
    sctx.vars.max_execution_time = 1;
    assert!(
        check(Some(&sctx.vars), Some(long_ago)).is_ok(),
        "not a SELECT"
    );
    sctx.vars.stmt_ctx_mut().in_select_stmt = true;
    let err = check(Some(&sctx.vars), Some(long_ago)).unwrap_err();
    assert!(
        err.to_string()
            .contains("maximum statement execution time exceeded"),
        "{err}"
    );
    assert!(check(Some(&sctx.vars), None).is_ok(), "no process info");
    let future = SystemTime::now() + Duration::from_secs(60);
    assert!(
        check(Some(&sctx.vars), Some(future)).is_ok(),
        "start time ahead"
    );
    sctx.vars.max_execution_time = 60_000;
    assert!(
        check(Some(&sctx.vars), Some(long_ago)).is_ok(),
        "within the limit"
    );
    sctx.vars.max_execution_time = 0;
    assert!(check(Some(&sctx.vars), Some(long_ago)).is_ok(), "no limit");
}

/// `globalPanicOnExceed`: the message by the tracker's label (Go's panic value, here the
/// error), at the panic priority.
#[test]
fn global_panic_on_exceed_messages() {
    use crate::select::GlobalPanicOnExceed;
    use tidb_memory::action::{ActionOnExceed, DefPanicPriority};
    use tidb_memory::tracker::{
        LabelForGlobalAnalyzeMemory, LabelForGlobalMemory, LabelForGlobalStorage, new_tracker,
    };

    let a = GlobalPanicOnExceed::default();
    assert_eq!(a.get_priority(), DefPanicPriority);
    for (label, msg) in [
        (
            LabelForGlobalStorage,
            "Out Of Quota For Local Temporary Space!",
        ),
        (LabelForGlobalMemory, "Out Of Global Memory Limit!"),
        (
            LabelForGlobalAnalyzeMemory,
            "Out Of Global Analyze Memory Limit!",
        ),
        (-1, "Out of Unknown Resource Quota!"),
    ] {
        let err = a.action(&new_tracker(label, -1)).unwrap_err();
        assert_eq!(err.to_string(), msg);
    }
}
