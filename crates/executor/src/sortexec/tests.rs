//! The non-spill unit tests of Go's `sortexec`: `TestInterruptedDuringSort`
//! (sortexec_pkg_test.go), `onePartitionAndAllDataInMemoryCase` (sort_spill_test.go),
//! `topNNoSpillCase` (topn_spill_test.go), `TestParallelSort` and `TestFailpoint`
//! (parallel_sort_test.go, without failpoints) with Go's result checker, and the executor
//! paths they do not reach (unsupported sort keys, count 0, offsets past the end, compaction
//! of the top-n heap; the parallel sort's tasks ending on close, on an error, on an early
//! stop and on a drop, a kill signal, both runtime topologies).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use async_trait::async_trait;
use tidb_base::Error;
use tidb_base::gostd::sort::sort;
use tidb_base::mysql::{TypeGeometry, TypeLonglong, TypeVarString};
use tidb_chunk::chunk::{self, Chunk};
use tidb_chunk::compare::{CompareFunc, get_compare_func};
use tidb_chunk::list::RowPtr;
use tidb_chunk::row::Row;
use tidb_expression::schema::new_schema;
use tidb_expression::{Column, Expr, ExprRef, Schema, new_one};
use tidb_memory::sqlkiller::{QueryInterrupted, SqlKiller};
use tidb_memory::tracker::{LabelForSQLText, Tracker, new_tracker};
use tidb_parser::types::FieldType;
use tidb_planner::core::physical_plans::PhysicalLimit;
use tidb_planner::util::byitem::ByItems;
use tidb_sessionctx::stmtctx::new_stmt_ctx;
use tidb_sessionctx::variable::session::{GlobalVarAccessor, SessionVars};
use tidb_types::field_type::new_field_type;

use super::sort::{SortExec, new_sort_exec};
use super::sort_partition::new_sort_partition;
use super::topn::{TopNExec, new_top_n_exec};
use crate::exec::ExecCtx;
use crate::internal::exec::executor::{
    BaseExecutorV2, Executor, new_base_executor, new_base_executor_v2, new_first_chunk,
};

struct NoGlobals;

impl GlobalVarAccessor for NoGlobals {
    fn get_global_sys_var(&self, _: &str) -> Result<String, Error> {
        Ok(String::new())
    }
    fn set_global_sys_var_only(&mut self, _: &str, _: &str, _: bool) -> Result<(), Error> {
        Ok(())
    }
    fn get_tidb_table_value(&self, _: &str) -> Result<String, Error> {
        Ok(String::new())
    }
    fn set_tidb_table_value(&mut self, _: &str, _: &str, _: &str) -> Result<(), Error> {
        Ok(())
    }
}

fn vars(chunk_size: i64) -> SessionVars {
    let mut vars = SessionVars::new(Box::new(NoGlobals));
    vars.init_chunk_size = chunk_size;
    vars.max_chunk_size = chunk_size;
    vars
}

/// Go's `testutil.MockDataSource`: returns its rows, at most the required rows per `Next`.
struct MockDataSource {
    base: BaseExecutorV2,
    data: Chunk,
    next_row: usize,
    nexts: Arc<AtomicUsize>,
    opened: Arc<AtomicBool>,
    /// `Next` fails once it has been called this many times.
    fail_after: Option<usize>,
    /// Dropped with the source (its strong count tells whether the source is alive).
    _alive: Arc<()>,
}

#[async_trait]
impl Executor for MockDataSource {
    fn base(&self) -> &BaseExecutorV2 {
        &self.base
    }
    fn base_mut(&mut self) -> &mut BaseExecutorV2 {
        &mut self.base
    }
    async fn open(&mut self, _ctx: &ExecCtx) -> Result<(), Error> {
        self.opened.store(true, Ordering::SeqCst);
        self.next_row = 0;
        Ok(())
    }
    async fn next(&mut self, _ctx: &ExecCtx, req: &mut Chunk) -> Result<(), Error> {
        let n = self.nexts.fetch_add(1, Ordering::SeqCst);
        if self.fail_after.is_some_and(|f| n >= f) {
            return Err(Error::plain("mock child failed"));
        }
        req.reset();
        while self.next_row < self.data.num_rows() && req.num_rows() < req.required_rows() {
            req.append_row(self.data.get_row(self.next_row));
            self.next_row += 1;
        }
        Ok(())
    }
    async fn close(&mut self) -> Result<(), Error> {
        self.opened.store(false, Ordering::SeqCst);
        Ok(())
    }
}

/// The columns of the test data: two BIGINT keys and a string payload (Go's `SortCase` has
/// BIGINT, DOUBLE, VARCHAR and DECIMAL columns; the govec vectors cover the other types).
fn field_types() -> Vec<FieldType> {
    vec![
        new_field_type(TypeLonglong),
        new_field_type(TypeLonglong),
        new_field_type(TypeVarString),
    ]
}

fn schema() -> Schema {
    let cols = field_types()
        .into_iter()
        .enumerate()
        .map(|(i, ft)| {
            Arc::new(Column {
                unique_id: i as i64 + 1,
                index: i as isize,
                ret_type: Arc::new(ft),
                ..Column::default()
            })
        })
        .collect();
    new_schema(cols)
}

/// `rows` rows of pseudo-random keys with some NULLs (a fixed linear congruential sequence).
fn gen_data(rows: usize, with_nulls: bool) -> Chunk {
    let fts = field_types();
    let mut chk = chunk::new(&fts, rows, rows.max(1));
    let mut x: u64 = 0x2545_f491_4f6c_dd1d;
    for i in 0..rows {
        x = x
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let a = ((x >> 33) % 50) as i64 - 25;
        let b = ((x >> 13) % 1_000_000) as i64;
        if with_nulls && (x >> 60) == 0 {
            chk.append_null(0);
        } else {
            chk.append_int64(0, a);
        }
        if with_nulls && (x >> 7).is_multiple_of(97) {
            chk.append_null(1);
        } else {
            chk.append_int64(1, b);
        }
        chk.append_string(2, format!("row{i}").as_bytes());
    }
    chk
}

struct Child {
    exec: Box<dyn Executor>,
    nexts: Arc<AtomicUsize>,
    opened: Arc<AtomicBool>,
    alive: Arc<()>,
}

fn data_source(vars: &SessionVars, data: Chunk) -> Child {
    failing_data_source(vars, data, None)
}

fn failing_data_source(vars: &SessionVars, data: Chunk, fail_after: Option<usize>) -> Child {
    let nexts = Arc::new(AtomicUsize::new(0));
    let opened = Arc::new(AtomicBool::new(false));
    let alive = Arc::new(());
    let exec = Box::new(MockDataSource {
        base: new_base_executor_v2(vars, &SqlKiller::new(), Some(schema()), 1, Vec::new()),
        data,
        next_row: 0,
        nexts: Arc::clone(&nexts),
        opened: Arc::clone(&opened),
        fail_after,
        _alive: Arc::clone(&alive),
    });
    Child {
        exec,
        nexts,
        opened,
        alive,
    }
}

fn col(i: usize) -> ExprRef {
    let s = schema();
    Expr::Column((*s.columns[i]).clone()).into_ref()
}

fn by_items(items: &[(ExprRef, bool)]) -> Vec<ByItems> {
    items
        .iter()
        .map(|(expr, desc)| ByItems {
            expr: Arc::clone(expr),
            desc: *desc,
        })
        .collect()
}

fn build_sort_exec(vars: &SessionVars, child: Box<dyn Executor>, items: Vec<ByItems>) -> SortExec {
    build_sort_exec_with_killer(vars, child, items, &SqlKiller::new())
}

fn build_sort_exec_with_killer(
    vars: &SessionVars,
    child: Box<dyn Executor>,
    items: Vec<ByItems>,
    killer: &Arc<SqlKiller>,
) -> SortExec {
    new_sort_exec(
        new_base_executor(vars, killer, Some(schema()), 2, vec![child]),
        items,
        schema(),
        tidb_sessionctx::exprstatic::new_expr_context(Vec::new()),
        1 << 30,
        vars.executor_concurrency,
    )
}

fn build_top_n_exec(
    vars: &SessionVars,
    child: Box<dyn Executor>,
    items: Vec<ByItems>,
    offset: u64,
    count: u64,
) -> TopNExec {
    new_top_n_exec(
        build_sort_exec(vars, child, items),
        PhysicalLimit {
            offset,
            count,
            ..PhysicalLimit::default()
        },
        4,
    )
}

/// An execution context whose statement tracker hangs under a session tracker (as Go's
/// tests set `StmtCtx.MemTracker` under `MemTracker`).
fn exec_ctx() -> (ExecCtx, Arc<Tracker>) {
    let session = new_tracker(LabelForSQLText, -1);
    let mut stmt = new_stmt_ctx();
    let stmt_tracker = new_tracker(LabelForSQLText, -1);
    stmt_tracker.attach_to(&session).unwrap();
    stmt.mem_tracker = Some(Arc::clone(&stmt_tracker));
    (ExecCtx::new(Arc::new(stmt)), stmt_tracker)
}

async fn execute(exe: &mut dyn Executor, ctx: &ExecCtx) -> Result<Vec<Chunk>, Error> {
    let mut res = Vec::new();
    let mut chk = new_first_chunk(exe);
    loop {
        exe.next(ctx, &mut chk).await?;
        if chk.num_rows() == 0 {
            break;
        }
        res.push(chk.copy_construct());
    }
    Ok(res)
}

/// Go's `resultChecker`: sorts the source rows with the executor's sort keys (the same
/// `sort.Slice`), takes `[offset, offset+count)`, and compares the result rows with them.
fn check(
    data: &Chunk,
    key_columns: &[usize],
    key_cmp_funcs: &[CompareFunc],
    descs: &[bool],
    result: &[Chunk],
    offset: i64,
    count: i64,
) -> bool {
    let less_row = |row_i: Row<'_>, row_j: Row<'_>| {
        for (i, &col_idx) in key_columns.iter().enumerate() {
            let mut cmp = key_cmp_funcs[i](row_i, col_idx, row_j, col_idx);
            if descs[i] {
                cmp = -cmp;
            }
            if cmp < 0 {
                return true;
            } else if cmp > 0 {
                return false;
            }
        }
        false
    };
    let mut ptrs: Vec<RowPtr> = (0..data.num_rows())
        .map(|i| RowPtr {
            chk_idx: 0,
            row_idx: i as u32,
        })
        .collect();
    sort(&mut ptrs, |a, b| {
        less_row(
            data.get_row(a.row_idx as usize),
            data.get_row(b.row_idx as usize),
        )
    });
    let offset = offset.max(0);
    let count = if count < 0 {
        ptrs.len() as i64 - offset
    } else {
        count
    };
    let start = (ptrs.len() as i64).min(offset) as usize;
    let end = (ptrs.len() as i64).min(offset + count) as usize;
    let ptrs = &ptrs[start..end];

    let total: usize = result.iter().map(|c| c.num_rows()).sum();
    if total != ptrs.len() {
        return false;
    }
    let mut cursor = 0;
    for chk in result {
        for i in 0..chk.num_rows() {
            if row_string(chk.get_row(i)) != row_string(data.get_row(ptrs[cursor].row_idx as usize))
            {
                return false;
            }
            cursor += 1;
        }
    }
    true
}

fn row_string(row: Row<'_>) -> String {
    let int = |c| {
        if row.is_null(c) {
            "<nil>".to_string()
        } else {
            row.get_int64(c).to_string()
        }
    };
    format!(
        "{}, {}, {}",
        int(0),
        int(1),
        String::from_utf8_lossy(row.get_string(2))
    )
}

fn cmp_funcs(n: usize) -> Vec<CompareFunc> {
    (0..n)
        .map(|_| get_compare_func(&new_field_type(TypeLonglong)).unwrap())
        .collect()
}

/// `onePartitionAndAllDataInMemoryCase`: 2048 rows, max chunk size 32, unparallel mode.
#[tokio::test]
async fn one_partition_and_all_data_in_memory() {
    let vars = vars(32);
    let data = gen_data(2048, true);
    let child = data_source(&vars, data.clone());
    let mut exe = build_sort_exec(
        &vars,
        child.exec,
        by_items(&[(col(0), false), (col(1), true)]),
    );
    exe.set_unparallel_for_test();
    let (ctx, stmt_tracker) = exec_ctx();
    exe.open(&ctx).await.unwrap();
    assert!(child.opened.load(Ordering::SeqCst));
    exe.init_unparallel_mode_for_test();
    let result = execute(&mut exe, &ctx).await.unwrap();

    assert_eq!(exe.get_sort_partition_list_len_for_test(), 1);
    assert!(!exe.is_spill_triggered_in_one_partition_for_test(0));
    assert_eq!(exe.get_row_num_in_one_partition_memory_for_test(0), 2048);
    assert!(result.iter().all(|c| c.num_rows() <= 32));
    assert!(stmt_tracker.bytes_consumed() > 0);
    exe.close().await.unwrap();
    assert!(!child.opened.load(Ordering::SeqCst));
    assert_eq!(stmt_tracker.bytes_consumed(), 0);

    let keys = exe.sort_keys();
    assert!(check(
        &data,
        &keys.key_columns,
        &keys.key_cmp_funcs,
        &keys.descs,
        &result,
        -1,
        -1
    ));
}

/// An empty child: no partition rows, no result.
#[tokio::test]
async fn sort_of_no_rows() {
    let vars = vars(32);
    let child = data_source(&vars, gen_data(0, false));
    let mut exe = build_sort_exec(&vars, child.exec, by_items(&[(col(0), false)]));
    exe.set_unparallel_for_test();
    let (ctx, _) = exec_ctx();
    exe.open(&ctx).await.unwrap();
    assert!(execute(&mut exe, &ctx).await.unwrap().is_empty());
    assert_eq!(exe.get_row_num_in_one_partition_memory_for_test(0), 0);
    exe.close().await.unwrap();
}

/// A constant by-item is skipped (`buildKeyColumns`); another expression is an error, and so
/// is a key type without a compare function (`initCompareFuncs`).
#[tokio::test]
async fn key_columns_and_compare_funcs() {
    let vars = vars(32);
    let data = gen_data(100, false);
    let child = data_source(&vars, data.clone());
    let constant = Expr::Constant(new_one()).into_ref();
    let mut exe = build_sort_exec(
        &vars,
        child.exec,
        by_items(&[(col(1), true), (constant, false)]),
    );
    exe.set_unparallel_for_test();
    let (ctx, _) = exec_ctx();
    exe.open(&ctx).await.unwrap();
    let result = execute(&mut exe, &ctx).await.unwrap();
    let keys = exe.sort_keys();
    assert_eq!(&*keys.key_columns, &[1]);
    assert_eq!(keys.key_cmp_funcs.len(), 2);
    assert!(check(
        &data,
        &keys.key_columns,
        &keys.key_cmp_funcs,
        &keys.descs,
        &result,
        -1,
        -1
    ));
    exe.close().await.unwrap();

    let child = data_source(&vars, data.clone());
    let corr = Expr::CorrelatedColumn(tidb_expression::CorrelatedColumn {
        column: (*schema().columns[0]).clone(),
        slot: tidb_expression::CorrSlot(1),
    })
    .into_ref();
    let mut exe = build_sort_exec(&vars, child.exec, by_items(&[(corr, false)]));
    exe.set_unparallel_for_test();
    exe.open(&ctx).await.unwrap();
    let mut chk = new_first_chunk(&exe);
    let err = exe.next(&ctx, &mut chk).await.unwrap_err();
    assert_eq!(err.to_string(), "Get unexpected expression");
    exe.close().await.unwrap();

    let child = data_source(&vars, data);
    let mut geo = (*schema().columns[0]).clone();
    geo.ret_type = Arc::new(new_field_type(TypeGeometry));
    let mut exe = build_sort_exec(
        &vars,
        child.exec,
        by_items(&[(Expr::Column(geo).into_ref(), false)]),
    );
    exe.set_unparallel_for_test();
    exe.open(&ctx).await.unwrap();
    let err = exe.next(&ctx, &mut chk).await.unwrap_err();
    assert_eq!(err.to_string(), "Sort executor not supports type geometry");
    exe.close().await.unwrap();
}

/// `TestInterruptedDuringSort`, without the timer: the kill signal is sent before the sort,
/// and the check every `signalCheckpointForSort` comparisons returns it.
#[test]
fn interrupted_during_sort() {
    let root_tracker = new_tracker(-1, -1);
    root_tracker
        .is_root_tracker_of_sess
        .store(true, Ordering::SeqCst);
    let killer = SqlKiller::new();
    killer.conn_id.store(1, Ordering::SeqCst);
    root_tracker.set_killer(Some(Arc::clone(&killer)));

    let fts: Vec<Arc<FieldType>> = field_types().into_iter().map(Arc::new).collect();
    let mut sp = new_sort_partition(
        fts,
        Arc::from(vec![false]),
        Arc::from(vec![0usize]),
        Arc::from(cmp_funcs(1)),
        1, // always can spill
        String::new(),
    );
    sp.get_mem_tracker().attach_to(&root_tracker).unwrap();
    let chk = Arc::new(gen_data(1024, false));
    for _ in 0..16 {
        assert!(sp.add(&chk).unwrap());
    }
    killer.send_kill_signal(QueryInterrupted);
    let err = sp.sort().unwrap_err();
    assert!(
        tidb_base::dbterror::exeerrors::ErrQueryInterrupted.equal(&err),
        "{err}"
    );
    sp.close().unwrap();
    assert_eq!(root_tracker.bytes_consumed(), 0);
}

/// `topNNoSpillCase` with Go's offsets (`totalRowNum / 10`, `totalRowNum / 3`) and offset 0.
#[tokio::test]
async fn top_n_no_spill() {
    let total_row_num = 2048u64;
    for (offset, count) in [
        (0, total_row_num / 3),
        (total_row_num / 10, total_row_num / 3),
        (total_row_num - 5, 100),
        (0, total_row_num + 7),
        (3, 1),
    ] {
        let vars = vars(32);
        let data = gen_data(total_row_num as usize, true);
        let child = data_source(&vars, data.clone());
        let mut exe = build_top_n_exec(
            &vars,
            child.exec,
            by_items(&[(col(0), true), (col(1), false)]),
            offset,
            count,
        );
        let (ctx, stmt_tracker) = exec_ctx();
        exe.open(&ctx).await.unwrap();
        let result = execute(&mut exe, &ctx).await.unwrap();
        assert!(!exe.is_spill_triggered_for_test());
        assert!(!exe.get_is_spill_triggered_in_stage1_for_test());
        assert!(!exe.get_is_spill_triggered_in_stage2_for_test());
        exe.close().await.unwrap();
        assert_eq!(stmt_tracker.bytes_consumed(), 0);
        let keys = exe.sort.sort_keys();
        assert!(
            check(
                &data,
                &keys.key_columns,
                &keys.key_cmp_funcs,
                &keys.descs,
                &result,
                offset as i64,
                count as i64
            ),
            "offset {offset} count {count}"
        );
    }
}

/// Count 0: no row and the child is never read; an offset past the end: no row.
#[tokio::test]
async fn top_n_count_zero_and_offset_past_the_end() {
    let vars = vars(32);
    let child = data_source(&vars, gen_data(100, false));
    let nexts = Arc::clone(&child.nexts);
    let mut exe = build_top_n_exec(&vars, child.exec, by_items(&[(col(0), false)]), 0, 0);
    let (ctx, _) = exec_ctx();
    exe.open(&ctx).await.unwrap();
    assert!(execute(&mut exe, &ctx).await.unwrap().is_empty());
    assert_eq!(nexts.load(Ordering::SeqCst), 0);
    exe.close().await.unwrap();

    let child = data_source(&vars, gen_data(100, false));
    let mut exe = build_top_n_exec(&vars, child.exec, by_items(&[(col(0), false)]), 100, 10);
    exe.open(&ctx).await.unwrap();
    assert!(execute(&mut exe, &ctx).await.unwrap().is_empty());
    exe.close().await.unwrap();
}

/// A rank top-n (the planner's `PrefixCol`) stops with `Blocked` in `Open`, before its child
/// opens.
#[tokio::test]
async fn rank_top_n_stops_in_open() {
    let vars = vars(32);
    let child = data_source(&vars, gen_data(10, false));
    let opened = Arc::clone(&child.opened);
    let mut exe = build_top_n_exec(&vars, child.exec, by_items(&[(col(2), false)]), 0, 3);
    exe.rank_info.truncate_key_exprs = vec![col(2)];
    exe.rank_info.truncate_key_prefix_char_counts = vec![2];
    let (ctx, _) = exec_ctx();
    let err = exe.open(&ctx).await.unwrap_err();
    assert_eq!(tidb_base::blocked::blocked_by(&err), Some("D12.5"));
    assert!(!opened.load(Ordering::SeqCst));
    exe.close().await.unwrap();
}

/// Ascending data and a descending top-n: every row replaces the heap's max, so the list
/// grows past four times the heap and is compacted (`doCompaction`).
#[tokio::test]
async fn top_n_compaction() {
    let vars = vars(32);
    let fts = field_types();
    let mut data = chunk::new(&fts, 1000, 1000);
    for i in 0..1000 {
        data.append_int64(0, i);
        data.append_int64(1, -i);
        data.append_string(2, format!("row{i}").as_bytes());
    }
    let child = data_source(&vars, data.clone());
    let mut exe = build_top_n_exec(&vars, child.exec, by_items(&[(col(0), true)]), 2, 10);
    let (ctx, _) = exec_ctx();
    exe.open(&ctx).await.unwrap();
    let result = execute(&mut exe, &ctx).await.unwrap();
    let got: Vec<i64> = result
        .iter()
        .flat_map(|c| (0..c.num_rows()).map(move |i| c.get_row(i).get_int64(0)))
        .collect();
    assert_eq!(got, (988..=997).rev().collect::<Vec<_>>());
    exe.close().await.unwrap();
}

/// The required rows of the parent's chunk bound each `Next` of the top-n.
#[tokio::test]
async fn top_n_respects_required_rows() {
    let vars = vars(32);
    let child = data_source(&vars, gen_data(200, false));
    let mut exe = build_top_n_exec(&vars, child.exec, by_items(&[(col(1), false)]), 0, 50);
    let (ctx, _) = exec_ctx();
    exe.open(&ctx).await.unwrap();
    let mut chk = new_first_chunk(&exe);
    chk.set_required_rows(7, 32);
    exe.next(&ctx, &mut chk).await.unwrap();
    assert_eq!(chk.num_rows(), 7);
    exe.close().await.unwrap();
}

/// Go's `resultChecker` for the parallel sort, which compares whole rows; the rows here have
/// a payload column that is not a sort key, so rows with equal keys come in the order the
/// workers were scheduled (as in Go). The check is: the same rows (as a multiset), in
/// non-decreasing key order.
fn check_parallel(data: &Chunk, exe: &SortExec, result: &[Chunk]) -> bool {
    let (key_columns, key_cmp_funcs, descs) = exe.get_sort_meta_for_test();
    let keys = super::sort::SortKeys {
        key_columns,
        key_cmp_funcs,
        descs: Arc::from(descs),
    };
    let mut got = Vec::new();
    let mut prev: Option<(usize, usize)> = None;
    for (c, chk) in result.iter().enumerate() {
        for i in 0..chk.num_rows() {
            if let Some((pc, pi)) = prev
                && keys.compare_row(result[pc].get_row(pi), chk.get_row(i)) > 0
            {
                return false;
            }
            prev = Some((c, i));
            got.push(row_string(chk.get_row(i)));
        }
    }
    let mut want: Vec<String> = (0..data.num_rows())
        .map(|i| row_string(data.get_row(i)))
        .collect();
    got.sort();
    want.sort();
    got == want
}

/// Go's `parallelSortTest`: open, read every row, close, check.
async fn parallel_sort_round(
    exe: &mut SortExec,
    ctx: &ExecCtx,
    data: &Chunk,
    stmt_tracker: &Tracker,
) {
    exe.open(ctx).await.unwrap();
    assert!(!exe.is_unparallel);
    let result = execute(exe, ctx).await.unwrap();
    assert!(result.iter().all(|c| c.num_rows() <= 32));
    exe.close().await.unwrap();
    assert!(check_parallel(data, exe, &result));
    assert_eq!(exe.parallel_state_for_test(), (0, 0, 1));
    assert!(!exe.is_spill_triggered_in_parallel_sort_for_test());
    assert_eq!(exe.get_spilled_row_num_in_parallel_sort_for_test(), 0);
    assert_eq!(stmt_tracker.bytes_consumed(), 0);
}

/// `TestParallelSort` without its failpoints (slow workers, a kill check every few
/// comparisons): 30000 rows, max chunk size 32, a new executor and a reused one per round,
/// with several worker counts (`tidb_executor_concurrency`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_sort() {
    let data = gen_data(30000, true);
    for concurrency in [5, 1, 2, 8] {
        let mut vars = vars(32);
        vars.executor_concurrency = concurrency;
        let items = by_items(&[(col(0), false), (col(1), true)]);
        let reused_child = data_source(&vars, data.clone());
        let mut reused = build_sort_exec(&vars, reused_child.exec, items.clone());
        let (ctx, stmt_tracker) = exec_ctx();
        for _ in 0..3 {
            let child = data_source(&vars, data.clone());
            let mut exe = build_sort_exec(&vars, child.exec, items.clone());
            parallel_sort_round(&mut exe, &ctx, &data, &stmt_tracker).await;
            assert!(!child.opened.load(Ordering::SeqCst));
            parallel_sort_round(&mut reused, &ctx, &data, &stmt_tracker).await;
            assert!(!reused_child.opened.load(Ordering::SeqCst));
        }
    }
}

/// `TestFailpoint` without its failpoints (random errors, slow workers, kill checks): 20
/// workers (twice the result channel's capacity) over 65536 rows, a new executor and a
/// reused one per round; also with a single key column, so that many rows tie.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_sort_many_workers() {
    let data = gen_data(65536, false);
    let mut vars = vars(32);
    vars.executor_concurrency = (super::sort::ResultChannelCapacity * 2) as i64;
    for items in [
        by_items(&[(col(0), false), (col(1), false)]),
        by_items(&[(col(0), true)]),
    ] {
        let reused_child = data_source(&vars, data.clone());
        let mut reused = build_sort_exec(&vars, reused_child.exec, items.clone());
        let (ctx, stmt_tracker) = exec_ctx();
        for _ in 0..2 {
            let child = data_source(&vars, data.clone());
            let mut exe = build_sort_exec(&vars, child.exec, items.clone());
            parallel_sort_round(&mut exe, &ctx, &data, &stmt_tracker).await;
            parallel_sort_round(&mut reused, &ctx, &data, &stmt_tracker).await;
        }
    }
}

/// The parallel sort on the N3 runtime's ExecPool, in both topologies (the tasks of a
/// NoSteal runtime stay on the connection's thread).
#[test]
fn parallel_sort_on_the_exec_pool() {
    for topology in [
        tidb_runtime::pool::Topology::Steal,
        tidb_runtime::pool::Topology::NoSteal,
    ] {
        let rt = tidb_runtime::pool::Runtime::new(tidb_runtime::pool::RuntimeConfig {
            topology,
            worker_threads: 2,
            ..Default::default()
        })
        .unwrap();
        let pool = rt.pool().clone();
        rt.block_on(async move {
            let data = gen_data(5000, true);
            let mut vars = vars(32);
            vars.executor_concurrency = 3;
            let child = data_source(&vars, data.clone());
            let mut exe = build_sort_exec(&vars, child.exec, by_items(&[(col(1), false)]));
            let (mut ctx, stmt_tracker) = exec_ctx();
            ctx.pool = Some(pool);
            parallel_sort_round(&mut exe, &ctx, &data, &stmt_tracker).await;
        });
    }
}

/// An empty child, and `Close` right after `Open` (no task was started).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parallel_sort_of_no_rows_and_close_without_next() {
    let vars = vars(32);
    let child = data_source(&vars, gen_data(0, false));
    let mut exe = build_sort_exec(&vars, child.exec, by_items(&[(col(0), false)]));
    let (ctx, _) = exec_ctx();
    exe.open(&ctx).await.unwrap();
    assert!(execute(&mut exe, &ctx).await.unwrap().is_empty());
    exe.close().await.unwrap();
    assert_eq!(exe.parallel_state_for_test(), (0, 0, 1));

    exe.open(&ctx).await.unwrap();
    assert!(child.opened.load(Ordering::SeqCst));
    let nexts = child.nexts.load(Ordering::SeqCst);
    exe.close().await.unwrap();
    assert!(!child.opened.load(Ordering::SeqCst));
    assert_eq!(child.nexts.load(Ordering::SeqCst), nexts);
    assert_eq!(exe.parallel_state_for_test(), (0, 0, 1));
}

/// An early stop: the reader takes one chunk and closes. `Close` stops the fetcher, the
/// workers and the generator through `finishCh`, drains the channels, awaits every task
/// (none is left), gets the child back and closes it, and the memory goes back to 0.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_sort_early_stop_ends_the_tasks() {
    for concurrency in [1, 4, 20] {
        let data = gen_data(20000, true);
        let mut vars = vars(32);
        vars.executor_concurrency = concurrency;
        let child = data_source(&vars, data.clone());
        let mut exe = build_sort_exec(&vars, child.exec, by_items(&[(col(1), false)]));
        let (ctx, stmt_tracker) = exec_ctx();
        exe.open(&ctx).await.unwrap();
        let mut chk = new_first_chunk(&exe);
        exe.next(&ctx, &mut chk).await.unwrap();
        assert_eq!(chk.num_rows(), 32);
        exe.close().await.unwrap();
        assert_eq!(exe.parallel_state_for_test(), (0, 0, 1));
        assert!(!child.opened.load(Ordering::SeqCst));
        assert_eq!(stmt_tracker.bytes_consumed(), 0);
        // The child is back in the executor and can run again.
        parallel_sort_round(&mut exe, &ctx, &data, &stmt_tracker).await;
    }
}

/// The child fails in the middle of the stream: `Next` returns its error (the fetcher sends
/// it before the generator starts), and `Close` still ends every task and closes the child.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_sort_child_error() {
    let vars = vars(32);
    let child = failing_data_source(&vars, gen_data(5000, false), Some(20));
    let mut exe = build_sort_exec(&vars, child.exec, by_items(&[(col(0), false)]));
    let (ctx, stmt_tracker) = exec_ctx();
    exe.open(&ctx).await.unwrap();
    let mut chk = new_first_chunk(&exe);
    let err = exe.next(&ctx, &mut chk).await.unwrap_err();
    assert_eq!(err.to_string(), "mock child failed");
    exe.close().await.unwrap();
    assert_eq!(exe.parallel_state_for_test(), (0, 0, 1));
    assert!(!child.opened.load(Ordering::SeqCst));
    assert_eq!(stmt_tracker.bytes_consumed(), 0);
}

/// A kill signal: the workers check it when they merge their sorted runs, and `Next` returns
/// the kill error; `Close` ends every task.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_sort_killed() {
    let mut vars = vars(32);
    vars.executor_concurrency = 4;
    let killer = SqlKiller::new();
    let child = data_source(&vars, gen_data(5000, false));
    let mut exe =
        build_sort_exec_with_killer(&vars, child.exec, by_items(&[(col(0), false)]), &killer);
    let (ctx, _) = exec_ctx();
    exe.open(&ctx).await.unwrap();
    killer.send_kill_signal(QueryInterrupted);
    let mut chk = new_first_chunk(&exe);
    let err = exe.next(&ctx, &mut chk).await.unwrap_err();
    assert!(
        tidb_base::dbterror::exeerrors::ErrQueryInterrupted.equal(&err),
        "{err}"
    );
    exe.close().await.unwrap();
    assert_eq!(exe.parallel_state_for_test(), (0, 0, 1));
    assert!(!child.opened.load(Ordering::SeqCst));
}

/// Dropping the executor without `Close` (the safety net of N3 3.5): its tasks are aborted
/// and the child the fetcher owns is dropped with them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_sort_drop_aborts_the_tasks() {
    let mut vars = vars(32);
    vars.executor_concurrency = 4;
    let child = data_source(&vars, gen_data(20000, false));
    let alive = child.alive;
    let mut exe = build_sort_exec(&vars, child.exec, by_items(&[(col(0), false)]));
    let (ctx, _) = exec_ctx();
    exe.open(&ctx).await.unwrap();
    let mut chk = new_first_chunk(&exe);
    exe.next(&ctx, &mut chk).await.unwrap();
    assert!(Arc::strong_count(&alive) > 1);
    drop(exe);
    for _ in 0..1000 {
        if Arc::strong_count(&alive) == 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert_eq!(Arc::strong_count(&alive), 1);
}

/// Outside a tokio runtime and without a pool the tasks cannot start: `Next` returns an
/// error and `Close` still closes the child.
#[test]
fn parallel_sort_without_a_runtime() {
    let vars = vars(32);
    let child = data_source(&vars, gen_data(10, false));
    let mut exe = build_sort_exec(&vars, child.exec, by_items(&[(col(0), false)]));
    let (ctx, _) = exec_ctx();
    futures::executor::block_on(async {
        exe.open(&ctx).await.unwrap();
        let mut chk = new_first_chunk(&exe);
        let err = exe.next(&ctx, &mut chk).await.unwrap_err();
        assert_eq!(
            err.to_string(),
            "the parallel sort needs a tokio runtime to run its tasks"
        );
        exe.close().await.unwrap();
    });
    assert!(!child.opened.load(Ordering::SeqCst));
}
