//! Go `pkg/executor/executor_required_rows_test.go`: the basic executors (limit, selection,
//! projection) and the stream aggregation (D12.3 part A) on a data source that checks how many rows each `Next` returns.
//!
//! Go generates random values; here the default generator counts (the values are never
//! checked, only the number of rows).

// Go names of constants (mapping rule M-GONAME).
#![allow(non_upper_case_globals)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use tidb_base::Error;
use tidb_base::mysql::{TypeDouble, TypeLonglong, TypeTiny};
use tidb_chunk::chunk::Chunk;
use tidb_expression::aggregation::{AggFuncDesc, new_agg_func_desc};
use tidb_expression::eval::RpnState;
use tidb_expression::evaluator::new_evaluator_suite;
use tidb_expression::expression::{EvalState, Expr};
use tidb_expression::scalar_function::ast_func as ast;
use tidb_expression::schema::new_schema;
use tidb_expression::{Column, Constant, ExprRef, Schema};
use tidb_memory::tracker::new_tracker;
use tidb_parser::types::FieldType;
use tidb_types::datum::new_int_datum;
use tidb_types::field_type::new_field_type;

use crate::adapter::TelemetryInfo;
use crate::adapter::tests::MockSession;
use crate::builder::new_executor_builder;
use crate::exec::ExecCtx;
use crate::glue::SessionContext;
use crate::internal::exec::executor::{
    self as exec, BaseExecutorV2, Executor, new_base_executor, new_base_executor_v2,
};
use crate::projection::{ProjectionExec, new_projection_executor_context};
use crate::select::{LimitExec, SelectionExec, new_selection_executor_context};

/// `variable.DefInitChunkSize`, `variable.DefMaxChunkSize`.
const DefInitChunkSize: i64 = 32;
const DefMaxChunkSize: i64 = 1024;

/// A value of the data source's generator, for a column of type Double or Longlong.
pub(crate) enum Value {
    Int(i64),
    Float(f64),
}

pub(crate) type Generator = Box<dyn FnMut(&FieldType) -> Value + Send>;

/// Go `requiredRowsDataSource`. The number of `Next` calls is shared with the test (the
/// data source moves into the executor tree).
pub(crate) struct RequiredRowsDataSource {
    pub(crate) base: BaseExecutorV2,
    total_rows: usize,
    count: usize,

    expected_rows_ret: Option<Vec<usize>>,
    num_next_called: Arc<AtomicUsize>,

    generator: Generator,
}

/// The data source's output: `[Double, Longlong]`.
pub(crate) fn data_source_schema() -> Schema {
    let ret_types = [new_field_type(TypeDouble), new_field_type(TypeLonglong)];
    let cols = ret_types
        .into_iter()
        .enumerate()
        .map(|(i, tp)| {
            Arc::new(Column {
                index: i as isize,
                ret_type: Arc::new(tp),
                ..Column::default()
            })
        })
        .collect();
    new_schema(cols)
}

pub(crate) fn new_required_rows_data_source_with_generator(
    ctx: &dyn SessionContext,
    total_rows: usize,
    expected_rows_ret: Option<Vec<usize>>,
    generator: Generator,
) -> (RequiredRowsDataSource, Arc<AtomicUsize>) {
    let num_next_called = Arc::new(AtomicUsize::new(0));
    let ds = RequiredRowsDataSource {
        base: new_base_executor_v2(
            ctx.get_session_vars(),
            &ctx.sql_killer(),
            Some(data_source_schema()),
            0,
            Vec::new(),
        ),
        total_rows,
        count: 0,
        expected_rows_ret,
        num_next_called: Arc::clone(&num_next_called),
        generator,
    };
    (ds, num_next_called)
}

pub(crate) fn new_required_rows_data_source(
    ctx: &dyn SessionContext,
    total_rows: usize,
    expected_rows_ret: Option<Vec<usize>>,
) -> (RequiredRowsDataSource, Arc<AtomicUsize>) {
    new_required_rows_data_source_with_generator(
        ctx,
        total_rows,
        expected_rows_ret,
        default_generator(),
    )
}

#[async_trait]
impl Executor for RequiredRowsDataSource {
    fn base(&self) -> &BaseExecutorV2 {
        &self.base
    }
    fn base_mut(&mut self) -> &mut BaseExecutorV2 {
        &mut self.base
    }
    async fn next(&mut self, _ctx: &ExecCtx, req: &mut Chunk) -> Result<(), Error> {
        req.reset();
        if self.count <= self.total_rows {
            let required = req.required_rows().min(self.total_rows - self.count);
            for _ in 0..required {
                self.gen_one_row(req);
            }
            self.count += required;
        }
        // Go checks in a deferred function.
        let called = self.num_next_called.fetch_add(1, Ordering::SeqCst);
        if let Some(expected_rows_ret) = &self.expected_rows_ret {
            let rows_ret = req.num_rows();
            let expected = expected_rows_ret[called];
            assert_eq!(
                rows_ret, expected,
                "unexpected number of rows returned, obtain: {rows_ret}, expected: {expected}"
            );
        }
        Ok(())
    }
}

impl RequiredRowsDataSource {
    fn gen_one_row(&mut self, req: &mut Chunk) {
        let types: Vec<FieldType> = exec::ret_types(self)
            .iter()
            .map(|t| (**t).clone())
            .collect();
        for (i, tp) in types.iter().enumerate() {
            match (self.generator)(tp) {
                Value::Int(v) => req.append_int64(i, v),
                Value::Float(v) => req.append_float64(i, v),
            }
        }
    }
}

/// Go draws random values; a counter is enough here (the values are never checked).
pub(crate) fn default_generator() -> Generator {
    let mut n = 0i64;
    Box::new(move |val_type: &FieldType| {
        n += 1;
        match val_type.get_type() {
            TypeLonglong => Value::Int(n),
            TypeDouble => Value::Float(n as f64),
            _ => panic!("not implement"),
        }
    })
}

pub(crate) fn check_num_next_called(
    num_next_called: &AtomicUsize,
    expected_rows_ret: &[usize],
) -> Result<(), String> {
    let n = num_next_called.load(Ordering::SeqCst);
    if n != expected_rows_ret.len() {
        return Err(format!(
            "unexpected number of call on Next, obtain: {n}, expected: {}",
            expected_rows_ret.len()
        ));
    }
    Ok(())
}

/// Go `defaultCtx()`: default chunk sizes, a statement memory tracker, snapshot ts 1 (no
/// disk tracker, no domain: nothing here reads them).
pub(crate) fn default_ctx() -> MockSession {
    let mut ctx = MockSession::new(1);
    let vars = &mut ctx.vars;
    vars.init_chunk_size = DefInitChunkSize;
    vars.max_chunk_size = DefMaxChunkSize;
    let quota = vars.mem_quota_query;
    vars.stmt_ctx_mut().mem_tracker = Some(new_tracker(-1, quota));
    vars.snapshot_ts = 1;
    ctx
}

pub(crate) fn exec_ctx(ctx: &MockSession) -> ExecCtx {
    ExecCtx::new(ctx.vars.stmt_ctx_arc())
}

#[tokio::test]
async fn limit_required_rows() {
    let max_chunk_size = DefMaxChunkSize as usize;
    struct Case {
        total_rows: usize,
        limit_offset: usize,
        limit_count: usize,
        required_rows: Vec<usize>,
        expected_rows: Vec<usize>,
        expected_rows_ds: Vec<usize>,
    }
    let test_cases = [
        Case {
            total_rows: 20,
            limit_offset: 0,
            limit_count: 10,
            required_rows: vec![3, 5, 1, 500, 500],
            expected_rows: vec![3, 5, 1, 1, 0],
            expected_rows_ds: vec![3, 5, 1, 1],
        },
        Case {
            total_rows: 20,
            limit_offset: 0,
            limit_count: 25,
            required_rows: vec![9, 500],
            expected_rows: vec![9, 11],
            expected_rows_ds: vec![9, 11],
        },
        Case {
            total_rows: 100,
            limit_offset: 50,
            limit_count: 30,
            required_rows: vec![10, 5, 10, 20],
            expected_rows: vec![10, 5, 10, 5],
            expected_rows_ds: vec![60, 5, 10, 5],
        },
        Case {
            total_rows: 100,
            limit_offset: 101,
            limit_count: 10,
            required_rows: vec![10],
            expected_rows: vec![0],
            expected_rows_ds: vec![100, 0],
        },
        Case {
            total_rows: max_chunk_size + 20,
            limit_offset: max_chunk_size + 1,
            limit_count: 10,
            required_rows: vec![3, 3, 3, 100],
            expected_rows: vec![3, 3, 3, 1],
            expected_rows_ds: vec![max_chunk_size, 4, 3, 3, 1],
        },
    ];

    for test_case in test_cases {
        let sctx = default_ctx();
        let ctx = exec_ctx(&sctx);
        let (ds, num_next_called) = new_required_rows_data_source(
            &sctx,
            test_case.total_rows,
            Some(test_case.expected_rows_ds.clone()),
        );
        let mut exe = build_limit_exec(
            &sctx,
            Box::new(ds),
            test_case.limit_offset,
            test_case.limit_count,
        );
        exe.open(&ctx).await.unwrap();
        let mut chk = exec::new_first_chunk(exe.as_ref());
        for i in 0..test_case.required_rows.len() {
            chk.set_required_rows(test_case.required_rows[i] as isize, max_chunk_size);
            exe.next(&ctx, &mut chk).await.unwrap();
            assert_eq!(test_case.expected_rows[i], chk.num_rows());
        }
        exe.close().await.unwrap();
        check_num_next_called(&num_next_called, &test_case.expected_rows_ds).unwrap();
    }
}

pub(crate) fn build_limit_exec(
    ctx: &dyn SessionContext,
    src: Box<dyn Executor>,
    offset: usize,
    count: usize,
) -> Box<dyn Executor> {
    let n = count.min(ctx.get_session_vars().max_chunk_size as usize);
    let mut base = new_base_executor(
        ctx.get_session_vars(),
        &ctx.sql_killer(),
        Some(src.schema()),
        0,
        vec![src],
    );
    base.base.alloc.set_init_cap(n as isize);
    Box::new(LimitExec {
        base,
        begin: offset as u64,
        end: (offset + count) as u64,
        cursor: 0,
        meet_first_batch: false,
        child_result: None,
        column_idxs_used_by_child: None,
        column_swap_helper: None,
    })
}

/// Go's `gen01`: the Longlong column alternates 0, 1, 0, ...
fn gen01() -> Generator {
    let mut closure_count = 0i64;
    Box::new(move |val_type: &FieldType| match val_type.get_type() {
        TypeLonglong => {
            let ret = closure_count % 2;
            closure_count += 1;
            Value::Int(ret)
        }
        TypeDouble => Value::Float(0.5),
        _ => panic!("not implement"),
    })
}

#[tokio::test]
async fn selection_required_rows() {
    let max_chunk_size = DefMaxChunkSize as usize;
    struct Case {
        total_rows: usize,
        filters_of_col1: i64,
        required_rows: Vec<usize>,
        expected_rows: Vec<usize>,
        expected_rows_ds: Vec<usize>,
        gen_: Option<Generator>,
    }
    let test_cases = vec![
        Case {
            total_rows: 20,
            filters_of_col1: 0,
            required_rows: vec![1, 2, 3, 4, 5, 20],
            expected_rows: vec![1, 2, 3, 4, 5, 5],
            expected_rows_ds: vec![20, 0],
            gen_: None,
        },
        Case {
            total_rows: 20,
            filters_of_col1: 0,
            required_rows: vec![1, 3, 5, 7, 9],
            expected_rows: vec![1, 3, 5, 1, 0],
            expected_rows_ds: vec![20, 0, 0],
            gen_: Some(gen01()),
        },
        Case {
            total_rows: max_chunk_size + 20,
            filters_of_col1: 1,
            required_rows: vec![1, 3, 5, max_chunk_size],
            expected_rows: vec![1, 3, 5, max_chunk_size / 2 - 1 - 3 - 5 + 10],
            expected_rows_ds: vec![max_chunk_size, 20, 0],
            gen_: Some(gen01()),
        },
    ];

    for test_case in test_cases {
        let sctx = default_ctx();
        let ctx = exec_ctx(&sctx);
        let mut filters: Vec<ExprRef> = Vec::new();
        let (ds, num_next_called) = match test_case.gen_ {
            // ignore filters
            None => new_required_rows_data_source(
                &sctx,
                test_case.total_rows,
                Some(test_case.expected_rows_ds.clone()),
            ),
            Some(gen_) => {
                let (ds, n) = new_required_rows_data_source_with_generator(
                    &sctx,
                    test_case.total_rows,
                    Some(test_case.expected_rows_ds.clone()),
                    gen_,
                );
                let col1 = Arc::clone(&ds.base.meta.schema().columns[1]);
                let f = eq_filter(&sctx, &col1, test_case.filters_of_col1);
                filters.push(f);
                (ds, n)
            }
        };
        let mut executor = build_selection_exec(&sctx, filters, Box::new(ds));
        executor.open(&ctx).await.unwrap();
        let mut chk = exec::new_first_chunk(executor.as_ref());
        for i in 0..test_case.required_rows.len() {
            chk.set_required_rows(test_case.required_rows[i] as isize, max_chunk_size);
            executor.next(&ctx, &mut chk).await.unwrap();
            assert_eq!(test_case.expected_rows[i], chk.num_rows());
        }
        executor.close().await.unwrap();
        check_num_next_called(&num_next_called, &test_case.expected_rows_ds).unwrap();
    }
}

/// `col = v` (Go: `expression.NewFunction(ctx, ast.EQ, ..., col, &Constant{v, TypeTiny})`).
pub(crate) fn eq_filter(sctx: &MockSession, col: &Arc<Column>, v: i64) -> ExprRef {
    tidb_expression::scalar_function::new_function(
        &*sctx.get_expr_ctx(),
        ast::EQ,
        new_field_type(TypeLonglong),
        vec![
            Expr::Column(Column::clone(col)).into_ref(),
            Expr::Constant(Constant::new(new_int_datum(v), new_field_type(TypeTiny))).into_ref(),
        ],
    )
    .unwrap()
}

pub(crate) fn build_selection_exec(
    ctx: &dyn SessionContext,
    filters: Vec<ExprRef>,
    src: Box<dyn Executor>,
) -> Box<dyn Executor> {
    Box::new(SelectionExec {
        ctx: new_selection_executor_context(ctx),
        base: new_base_executor_v2(
            ctx.get_session_vars(),
            &ctx.sql_killer(),
            Some(src.schema()),
            0,
            vec![src],
        ),
        batched: false,
        filters,
        selected: Vec::new(),
        input_row: None,
        child_result: None,
        mem_tracker: None,
        eval_state: EvalState::new(),
        rpn: RpnState::default(),
    })
}

#[tokio::test]
async fn projection_unparallel_required_rows() {
    let max_chunk_size = DefMaxChunkSize as usize;
    struct Case {
        total_rows: usize,
        required_rows: Vec<usize>,
        expected_rows: Vec<usize>,
        expected_rows_ds: Vec<usize>,
    }
    let test_cases = [
        Case {
            total_rows: 20,
            required_rows: vec![1, 3, 5, 7, 9],
            expected_rows: vec![1, 3, 5, 7, 4],
            expected_rows_ds: vec![1, 3, 5, 7, 4],
        },
        Case {
            total_rows: max_chunk_size + 10,
            required_rows: vec![1, 3, 5, 7, 9, max_chunk_size],
            expected_rows: vec![1, 3, 5, 7, 9, max_chunk_size - 1 - 3 - 5 - 7 - 9 + 10],
            expected_rows_ds: vec![1, 3, 5, 7, 9, max_chunk_size - 1 - 3 - 5 - 7 - 9 + 10],
        },
        Case {
            total_rows: max_chunk_size * 2 + 10,
            required_rows: vec![1, 7, 9, max_chunk_size, max_chunk_size + 10],
            expected_rows: vec![1, 7, 9, max_chunk_size, max_chunk_size + 10 - 1 - 7 - 9],
            expected_rows_ds: vec![1, 7, 9, max_chunk_size, max_chunk_size + 10 - 1 - 7 - 9],
        },
    ];

    for test_case in test_cases {
        let sctx = default_ctx();
        let ctx = exec_ctx(&sctx);
        let (ds, num_next_called) = new_required_rows_data_source(
            &sctx,
            test_case.total_rows,
            Some(test_case.expected_rows_ds.clone()),
        );
        let exprs = column_exprs(&ds.base.meta.schema());
        let mut executor = build_projection_exec(&sctx, exprs, Box::new(ds), 0);
        executor.open(&ctx).await.unwrap();
        let mut chk = exec::new_first_chunk(executor.as_ref());
        for i in 0..test_case.required_rows.len() {
            chk.set_required_rows(test_case.required_rows[i] as isize, max_chunk_size);
            executor.next(&ctx, &mut chk).await.unwrap();
            assert_eq!(test_case.expected_rows[i], chk.num_rows());
        }
        executor.close().await.unwrap();
        check_num_next_called(&num_next_called, &test_case.expected_rows_ds).unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "Go skips it too: not stable because of goroutine schedule"]
async fn projection_parallel_required_rows() {
    let max_chunk_size = DefMaxChunkSize as usize;
    struct Case {
        total_rows: usize,
        num_workers: i64,
        required_rows: Vec<usize>,
        expected_rows: Vec<usize>,
        expected_rows_ds: Vec<usize>,
    }
    let test_cases = [
        Case {
            total_rows: 20,
            num_workers: 1,
            required_rows: vec![1, 2, 3, 4, 5, 6, 1, 1],
            expected_rows: vec![1, 1, 2, 3, 4, 5, 4, 0],
            expected_rows_ds: vec![1, 1, 2, 3, 4, 5, 4, 0],
        },
        Case {
            total_rows: max_chunk_size * 2,
            num_workers: 1,
            required_rows: vec![7, max_chunk_size, max_chunk_size, max_chunk_size],
            expected_rows: vec![7, 7, max_chunk_size, max_chunk_size - 14],
            expected_rows_ds: vec![7, 7, max_chunk_size, max_chunk_size - 14, 0],
        },
        Case {
            total_rows: 20,
            num_workers: 2,
            required_rows: vec![1, 2, 3, 4, 5, 6, 1, 1, 1],
            expected_rows: vec![1, 1, 1, 2, 3, 4, 5, 3, 0],
            expected_rows_ds: vec![1, 1, 1, 2, 3, 4, 5, 3, 0],
        },
    ];

    for test_case in test_cases {
        let sctx = default_ctx();
        let ctx = exec_ctx(&sctx);
        let (ds, num_next_called) = new_required_rows_data_source(
            &sctx,
            test_case.total_rows,
            Some(test_case.expected_rows_ds.clone()),
        );
        let exprs = column_exprs(&ds.base.meta.schema());
        let mut executor = build_projection_exec(&sctx, exprs, Box::new(ds), test_case.num_workers);
        executor.open(&ctx).await.unwrap();
        let mut chk = exec::new_first_chunk(executor.as_ref());
        for i in 0..test_case.required_rows.len() {
            chk.set_required_rows(test_case.required_rows[i] as isize, max_chunk_size);
            executor.next(&ctx, &mut chk).await.unwrap();
            assert_eq!(test_case.expected_rows[i], chk.num_rows());

            // wait projectionInputFetcher blocked on fetching data
            // from child in the background.
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        executor.close().await.unwrap();
        check_num_next_called(&num_next_called, &test_case.expected_rows_ds).unwrap();
    }
}

/// The schema's columns as the projection's expressions.
pub(crate) fn column_exprs(schema: &Schema) -> Vec<ExprRef> {
    schema
        .columns
        .iter()
        .map(|col| Expr::Column(Column::clone(col)).into_ref())
        .collect()
}

pub(crate) fn build_projection_exec(
    ctx: &dyn SessionContext,
    exprs: Vec<ExprRef>,
    src: Box<dyn Executor>,
    num_workers: i64,
) -> Box<dyn Executor> {
    Box::new(ProjectionExec::new(
        new_projection_executor_context(ctx),
        new_base_executor_v2(
            ctx.get_session_vars(),
            &ctx.sql_killer(),
            Some(src.schema()),
            0,
            vec![src],
        ),
        num_workers,
        new_evaluator_suite(&exprs, false),
        false,
    ))
}

/// Go `divGenerator`: the integer value advances one step every `factor` rows. Go's double
/// branch reads the integer counter (and advances its own, unused one): kept.
fn div_generator(factor: i64) -> Generator {
    let mut closure_count_int = 0i64;
    let mut closure_count_double = 0i64;
    Box::new(move |val_type: &FieldType| match val_type.get_type() {
        TypeLonglong => {
            let ret = closure_count_int / factor;
            closure_count_int += 1;
            Value::Int(ret)
        }
        TypeDouble => {
            let ret = (closure_count_int / factor) as f64;
            closure_count_double += 1;
            let _ = closure_count_double;
            Value::Float(ret)
        }
        _ => panic!("not implement"),
    })
}

#[tokio::test]
async fn stream_agg_required_rows() {
    let max_chunk_size = DefMaxChunkSize as usize;
    struct Case {
        total_rows: usize,
        agg_func: &'static str,
        required_rows: Vec<usize>,
        expected_rows: Vec<usize>,
        expected_rows_ds: Vec<usize>,
        factor: i64,
    }
    let test_cases = [
        Case {
            total_rows: 1000000,
            agg_func: tidb_expression::ast_funcs::AggFuncSum,
            required_rows: vec![1, 2, 3, 4, 5, 6, 7],
            expected_rows: vec![1, 2, 3, 4, 5, 6, 7],
            expected_rows_ds: vec![max_chunk_size],
            factor: 1,
        },
        Case {
            total_rows: max_chunk_size * 3,
            agg_func: tidb_expression::ast_funcs::AggFuncAvg,
            required_rows: vec![1, 3],
            expected_rows: vec![1, 2],
            expected_rows_ds: vec![max_chunk_size, max_chunk_size, max_chunk_size, 0],
            factor: max_chunk_size as i64,
        },
        Case {
            total_rows: max_chunk_size * 2 - 1,
            agg_func: tidb_expression::ast_funcs::AggFuncMax,
            required_rows: vec![max_chunk_size / 2 + 1],
            expected_rows: vec![max_chunk_size / 2 + 1],
            expected_rows_ds: vec![max_chunk_size, max_chunk_size - 1],
            factor: 2,
        },
    ];

    for test_case in test_cases {
        let sctx = default_ctx();
        let ctx = exec_ctx(&sctx);
        let (ds, num_next_called) = new_required_rows_data_source_with_generator(
            &sctx,
            test_case.total_rows,
            Some(test_case.expected_rows_ds.clone()),
            div_generator(test_case.factor),
        );
        let child_cols = ds.base.meta.schema().columns.clone();
        let schema = new_schema(child_cols.clone());
        let group_by = vec![Expr::Column(Column::clone(&child_cols[1])).into_ref()];
        let agg_func = new_agg_func_desc(
            &*sctx.get_expr_ctx(),
            test_case.agg_func,
            vec![Expr::Column(Column::clone(&child_cols[0])).into_ref()],
            true,
        )
        .unwrap();
        let agg_funcs = vec![agg_func];
        let mut executor =
            build_stream_agg_executor(&sctx, Box::new(ds), &schema, &agg_funcs, &group_by);
        executor.open(&ctx).await.unwrap();
        let mut chk = exec::new_first_chunk(executor.as_ref());
        for i in 0..test_case.required_rows.len() {
            chk.set_required_rows(test_case.required_rows[i] as isize, max_chunk_size);
            executor.next(&ctx, &mut chk).await.unwrap();
            assert_eq!(test_case.expected_rows[i], chk.num_rows());
        }
        executor.close().await.unwrap();
        check_num_next_called(&num_next_called, &test_case.expected_rows_ds).unwrap();
    }
}

/// Go `buildStreamAggExecutor` (benchmark_test.go) with `concurrency` 1 and a sorted data
/// source, the only form `TestStreamAggRequiredRows` uses: the stream aggregation directly
/// over the data source (Go builds the plan over a mock data plan and runs the builder; the
/// plan, D10.1, is not ported, so the builder's function is called with its fields).
pub(crate) fn build_stream_agg_executor(
    ctx: &dyn SessionContext,
    src_exec: Box<dyn Executor>,
    schema: &Schema,
    agg_funcs: &[AggFuncDesc],
    group_items: &[ExprRef],
) -> Box<dyn Executor> {
    let mut b = new_executor_builder(ctx, None, Arc::new(TelemetryInfo::default()));
    let e = b
        .build_stream_agg_from_child_exec(ctx, Some(src_exec), schema, 0, group_items, agg_funcs)
        .expect("the aggregate functions of the test build");
    Box::new(e)
}
