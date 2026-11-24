//! The non-spill unit tests of Go's `sortexec`: `TestInterruptedDuringSort`
//! (sortexec_pkg_test.go), `onePartitionAndAllDataInMemoryCase` (sort_spill_test.go),
//! `topNNoSpillCase` (topn_spill_test.go) with Go's result checker, and the executor paths
//! they do not reach (the parallel path's boundary, unsupported sort keys, count 0, offsets
//! past the end, compaction of the top-n heap).

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
        self.nexts.fetch_add(1, Ordering::SeqCst);
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
}

fn data_source(vars: &SessionVars, data: Chunk) -> Child {
    let nexts = Arc::new(AtomicUsize::new(0));
    let opened = Arc::new(AtomicBool::new(false));
    let exec = Box::new(MockDataSource {
        base: new_base_executor_v2(vars, &SqlKiller::new(), Some(schema()), 1, Vec::new()),
        data,
        next_row: 0,
        nexts: Arc::clone(&nexts),
        opened: Arc::clone(&opened),
    });
    Child {
        exec,
        nexts,
        opened,
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
    new_sort_exec(
        new_base_executor(vars, &SqlKiller::new(), Some(schema()), 2, vec![child]),
        items,
        schema(),
        tidb_sessionctx::exprstatic::new_expr_context(Vec::new()),
        1 << 30,
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

/// Go's `Open` always takes the parallel path (sort.go:170): it stops there with `Blocked`
/// before the child is opened.
#[tokio::test]
async fn open_takes_the_parallel_path() {
    let vars = vars(32);
    let child = data_source(&vars, gen_data(10, false));
    let mut exe = build_sort_exec(&vars, child.exec, by_items(&[(col(0), false)]));
    let (ctx, _) = exec_ctx();
    let err = exe.open(&ctx).await.unwrap_err();
    assert_eq!(tidb_base::blocked::blocked_by(&err), Some("D12.5"));
    assert!(!exe.is_unparallel);
    assert!(!child.opened.load(Ordering::SeqCst));
    exe.close().await.unwrap();
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
