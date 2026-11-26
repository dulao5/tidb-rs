//! Ported Go unit tests for window functions and sliding windows (`TestLeadLag`,
//! `TestWindowFunctions`, `TestDequeReset`, `TestDequePushPop`, and memory-accounting
//! property tests for `TestMemPercentRank`, `TestMemCumeDist`, `TestMemNtile`, `TestMemRank`,
//! `TestMemRowNumber`, `TestMemValue`, `TestMemLeadLag`), plus regression tests for sliding
//! `Slide` methods and `LEAD` large `uint64` offset handling.

use std::sync::Arc;

use tidb_base::mysql::{
    self, TypeDate, TypeDouble, TypeDuration, TypeFloat, TypeJSON, TypeLong, TypeLonglong,
    TypeNewDecimal, TypeString, TypeTiDBVectorFloat32, TypeTiny, UnsignedFlag,
};
use tidb_chunk::ColAlloc;
use tidb_chunk::chunk::{Chunk, new_chunk_with_capacity};
use tidb_chunk::row::{Row, RowHandle};
use tidb_expression::aggregation::{AggFuncDesc, BaseFuncDesc, new_agg_func_desc};
use tidb_expression::ast_funcs as ast;
use tidb_expression::constant::{new_one, new_zero};
use tidb_expression::context::{BuildContext, EvalContext};
use tidb_expression::expression::EvalState;
use tidb_expression::{Column, Constant, Expr, ExprRef};
use tidb_memory::TrackedAlloc;
use tidb_memory::tracker::new_tracker;
use tidb_parser::types::FieldType;
use tidb_sessionctx::exprstatic as es;
use tidb_types::datum::{
    Datum, new_float64_datum, new_int_datum, new_string_datum, new_uint_datum,
};
use tidb_types::field_type::new_field_type;
use tidb_types::json::{JSONValue, create_binary_json};
use tidb_types::mydecimal::new_dec_from_int;
use tidb_types::time::{Duration, time_from_days};
use tidb_types::vector::must_create_vector_float32;

use super::{assert_datum_eq, data_gen, date, dec, dur, json, null};
use crate::aggfuncs::{build_window_functions, new_deque};

struct WindowTest {
    data_type: FieldType,
    num_rows: usize,
    func_name: &'static str,
    args: Vec<ExprRef>,
    order_by_cols: Vec<Column>,
    results: Vec<Datum>,
}

impl WindowTest {
    fn gen_src_chk(&self) -> Chunk {
        let mut chk = new_chunk_with_capacity(std::slice::from_ref(&self.data_type), self.num_rows);
        for i in 0..self.num_rows {
            chk.append_datum(0, &data_gen(&self.data_type, i));
        }
        chk
    }
}

fn build_window_tester_with_args(
    func_name: &'static str,
    tp: u8,
    extra_args: Vec<ExprRef>,
    order_by_cols_count: usize,
    num_rows: usize,
    results: Vec<Datum>,
) -> WindowTest {
    let data_type = new_field_type(tp);
    let mut args = Vec::new();
    if func_name != ast::WindowFuncNtile {
        args.push(
            Expr::Column(Column {
                ret_type: Arc::new(data_type.clone()),
                index: 0,
                ..Column::default()
            })
            .into_ref(),
        );
    }
    args.extend(extra_args);
    let mut order_by_cols = Vec::new();
    if order_by_cols_count > 0 {
        order_by_cols.push(Column {
            ret_type: Arc::new(data_type.clone()),
            index: 0,
            ..Column::default()
        });
    }
    WindowTest {
        data_type,
        num_rows,
        func_name,
        args,
        order_by_cols,
        results,
    }
}

fn build_window_tester(
    func_name: &'static str,
    tp: u8,
    constant_arg: u64,
    order_by_cols_count: usize,
    num_rows: usize,
    results: Vec<Datum>,
) -> WindowTest {
    let mut extra_args = Vec::new();
    if constant_arg > 0 {
        extra_args.push(
            Expr::Constant(Constant::new(
                new_uint_datum(constant_arg),
                new_field_type(TypeLonglong),
            ))
            .into_ref(),
        );
    }
    build_window_tester_with_args(
        func_name,
        tp,
        extra_args,
        order_by_cols_count,
        num_rows,
        results,
    )
}

fn run_window_test(p: &WindowTest) {
    let src_chk = p.gen_src_chk();
    let ctx = es::new_expr_context(vec![]);
    let ec = ctx.get_eval_ctx();
    let ec: &dyn EvalContext = &*ec;
    let st = EvalState::new();

    let desc = new_agg_func_desc(&*ctx, p.func_name, p.args.clone(), false).unwrap();
    let final_func = build_window_functions(&*ctx, &desc, 0, &p.order_by_cols)
        .unwrap()
        .expect("window function builds");
    let mut final_pr = final_func.alloc_partial_result();
    let mut result_chk = new_chunk_with_capacity(std::slice::from_ref(&*desc.base.ret_tp), 1);

    for i in 0..src_chk.num_rows() {
        let row = src_chk.get_row(i);
        final_func
            .update_partial_result(ec, &st, &[row], &mut final_pr)
            .unwrap();
    }
    drop(src_chk);

    assert_eq!(p.results.len(), p.num_rows);
    let collate = desc.base.ret_tp.get_collate();
    for want in &p.results {
        final_func
            .append_final_result2_chunk(ec, &mut final_pr, &mut result_chk)
            .unwrap();
        let got = result_chk.get_row(0).get_datum(0, &desc.base.ret_tp);
        assert_datum_eq(ec, &got, want, collate);
        result_chk.reset();
    }
    final_func.reset_partial_result(&mut final_pr);
}

#[test]
fn test_lead_lag() {
    let zero = Expr::Constant(new_zero()).into_ref();
    let one = Expr::Constant(new_one()).into_ref();
    let two = Expr::Constant(Constant::new(new_int_datum(2), new_field_type(TypeTiny))).into_ref();
    let three =
        Expr::Constant(Constant::new(new_int_datum(3), new_field_type(TypeTiny))).into_ref();
    let million = Expr::Constant(Constant::new(
        new_int_datum(1_000_000),
        new_field_type(TypeLong),
    ))
    .into_ref();
    let default_arg = Expr::Column(Column {
        ret_type: Arc::new(new_field_type(TypeLonglong)),
        index: 0,
        ..Column::default()
    })
    .into_ref();

    let num_rows = 3;
    let i = new_int_datum;
    let tests = vec![
        // lag(field0, N)
        build_window_tester_with_args(
            ast::WindowFuncLag,
            TypeLonglong,
            vec![Arc::clone(&zero)],
            0,
            num_rows,
            vec![i(0), i(1), i(2)],
        ),
        build_window_tester_with_args(
            ast::WindowFuncLag,
            TypeLonglong,
            vec![Arc::clone(&one)],
            0,
            num_rows,
            vec![null(), i(0), i(1)],
        ),
        build_window_tester_with_args(
            ast::WindowFuncLag,
            TypeLonglong,
            vec![Arc::clone(&two)],
            0,
            num_rows,
            vec![null(), null(), i(0)],
        ),
        build_window_tester_with_args(
            ast::WindowFuncLag,
            TypeLonglong,
            vec![Arc::clone(&three)],
            0,
            num_rows,
            vec![null(), null(), null()],
        ),
        build_window_tester_with_args(
            ast::WindowFuncLag,
            TypeLonglong,
            vec![Arc::clone(&million)],
            0,
            num_rows,
            vec![null(), null(), null()],
        ),
        // lag(field0, N, 1000000)
        build_window_tester_with_args(
            ast::WindowFuncLag,
            TypeLonglong,
            vec![Arc::clone(&zero), Arc::clone(&million)],
            0,
            num_rows,
            vec![i(0), i(1), i(2)],
        ),
        build_window_tester_with_args(
            ast::WindowFuncLag,
            TypeLonglong,
            vec![Arc::clone(&one), Arc::clone(&million)],
            0,
            num_rows,
            vec![i(1_000_000), i(0), i(1)],
        ),
        build_window_tester_with_args(
            ast::WindowFuncLag,
            TypeLonglong,
            vec![Arc::clone(&two), Arc::clone(&million)],
            0,
            num_rows,
            vec![i(1_000_000), i(1_000_000), i(0)],
        ),
        build_window_tester_with_args(
            ast::WindowFuncLag,
            TypeLonglong,
            vec![Arc::clone(&three), Arc::clone(&million)],
            0,
            num_rows,
            vec![i(1_000_000), i(1_000_000), i(1_000_000)],
        ),
        build_window_tester_with_args(
            ast::WindowFuncLag,
            TypeLonglong,
            vec![Arc::clone(&million), Arc::clone(&million)],
            0,
            num_rows,
            vec![i(1_000_000), i(1_000_000), i(1_000_000)],
        ),
        // lag(field0, N, field0)
        build_window_tester_with_args(
            ast::WindowFuncLag,
            TypeLonglong,
            vec![Arc::clone(&zero), Arc::clone(&default_arg)],
            0,
            num_rows,
            vec![i(0), i(1), i(2)],
        ),
        build_window_tester_with_args(
            ast::WindowFuncLag,
            TypeLonglong,
            vec![Arc::clone(&one), Arc::clone(&default_arg)],
            0,
            num_rows,
            vec![i(0), i(0), i(1)],
        ),
        build_window_tester_with_args(
            ast::WindowFuncLag,
            TypeLonglong,
            vec![Arc::clone(&two), Arc::clone(&default_arg)],
            0,
            num_rows,
            vec![i(0), i(1), i(0)],
        ),
        build_window_tester_with_args(
            ast::WindowFuncLag,
            TypeLonglong,
            vec![Arc::clone(&three), Arc::clone(&default_arg)],
            0,
            num_rows,
            vec![i(0), i(1), i(2)],
        ),
        build_window_tester_with_args(
            ast::WindowFuncLag,
            TypeLonglong,
            vec![Arc::clone(&million), Arc::clone(&default_arg)],
            0,
            num_rows,
            vec![i(0), i(1), i(2)],
        ),
        // lead(field0, N)
        build_window_tester_with_args(
            ast::WindowFuncLead,
            TypeLonglong,
            vec![Arc::clone(&zero)],
            0,
            num_rows,
            vec![i(0), i(1), i(2)],
        ),
        build_window_tester_with_args(
            ast::WindowFuncLead,
            TypeLonglong,
            vec![Arc::clone(&one)],
            0,
            num_rows,
            vec![i(1), i(2), null()],
        ),
        build_window_tester_with_args(
            ast::WindowFuncLead,
            TypeLonglong,
            vec![Arc::clone(&two)],
            0,
            num_rows,
            vec![i(2), null(), null()],
        ),
        build_window_tester_with_args(
            ast::WindowFuncLead,
            TypeLonglong,
            vec![Arc::clone(&three)],
            0,
            num_rows,
            vec![null(), null(), null()],
        ),
        build_window_tester_with_args(
            ast::WindowFuncLead,
            TypeLonglong,
            vec![Arc::clone(&million)],
            0,
            num_rows,
            vec![null(), null(), null()],
        ),
        // lead(field0, N, 1000000)
        build_window_tester_with_args(
            ast::WindowFuncLead,
            TypeLonglong,
            vec![Arc::clone(&zero), Arc::clone(&million)],
            0,
            num_rows,
            vec![i(0), i(1), i(2)],
        ),
        build_window_tester_with_args(
            ast::WindowFuncLead,
            TypeLonglong,
            vec![Arc::clone(&one), Arc::clone(&million)],
            0,
            num_rows,
            vec![i(1), i(2), i(1_000_000)],
        ),
        build_window_tester_with_args(
            ast::WindowFuncLead,
            TypeLonglong,
            vec![Arc::clone(&two), Arc::clone(&million)],
            0,
            num_rows,
            vec![i(2), i(1_000_000), i(1_000_000)],
        ),
        build_window_tester_with_args(
            ast::WindowFuncLead,
            TypeLonglong,
            vec![Arc::clone(&three), Arc::clone(&million)],
            0,
            num_rows,
            vec![i(1_000_000), i(1_000_000), i(1_000_000)],
        ),
        build_window_tester_with_args(
            ast::WindowFuncLead,
            TypeLonglong,
            vec![Arc::clone(&million), Arc::clone(&million)],
            0,
            num_rows,
            vec![i(1_000_000), i(1_000_000), i(1_000_000)],
        ),
        // lead(field0, N, field0)
        build_window_tester_with_args(
            ast::WindowFuncLead,
            TypeLonglong,
            vec![zero, Arc::clone(&default_arg)],
            0,
            num_rows,
            vec![i(0), i(1), i(2)],
        ),
        build_window_tester_with_args(
            ast::WindowFuncLead,
            TypeLonglong,
            vec![one, Arc::clone(&default_arg)],
            0,
            num_rows,
            vec![i(1), i(2), i(2)],
        ),
        build_window_tester_with_args(
            ast::WindowFuncLead,
            TypeLonglong,
            vec![two, Arc::clone(&default_arg)],
            0,
            num_rows,
            vec![i(2), i(1), i(2)],
        ),
        build_window_tester_with_args(
            ast::WindowFuncLead,
            TypeLonglong,
            vec![three, Arc::clone(&default_arg)],
            0,
            num_rows,
            vec![i(0), i(1), i(2)],
        ),
        build_window_tester_with_args(
            ast::WindowFuncLead,
            TypeLonglong,
            vec![million, default_arg],
            0,
            num_rows,
            vec![i(0), i(1), i(2)],
        ),
    ];
    for t in &tests {
        run_window_test(t);
    }
}

/// Regression test for large `uint64` offsets in `LEAD`: `cur_idx + offset` must not wrap
/// around and return an earlier row (#405). Covers offsets `u64::MAX` (`2^64-1`) and
/// `u64::MAX - 1` (`2^64-2`), with and without a default value, row 0 and wrap-around rows,
/// plus non-wrapping large offset `i64::MAX as u64` (`2^63-1`) and `LAG` as controls.
#[test]
fn lead_large_uint64_offset_does_not_wrap() {
    let mut u64_ft = new_field_type(TypeLonglong);
    u64_ft.add_flag(UnsignedFlag);

    let max_u64 =
        Expr::Constant(Constant::new(new_uint_datum(u64::MAX), u64_ft.clone())).into_ref();
    let max_u64_m1 =
        Expr::Constant(Constant::new(new_uint_datum(u64::MAX - 1), u64_ft.clone())).into_ref();
    let max_i64 = Expr::Constant(Constant::new(new_uint_datum(i64::MAX as u64), u64_ft)).into_ref();
    let def_minus_one = Expr::Constant(Constant::new(
        new_int_datum(-1),
        new_field_type(TypeLonglong),
    ))
    .into_ref();

    let num_rows = 5;
    let i = new_int_datum;
    let all_null = vec![null(), null(), null(), null(), null()];
    let all_def = vec![i(-1), i(-1), i(-1), i(-1), i(-1)];

    let tests = vec![
        // LEAD with 2^64 - 1 (no default): row 0 stays out-of-range, rows 1..4 would wrap to 0..3 in Go
        build_window_tester_with_args(
            ast::WindowFuncLead,
            TypeLonglong,
            vec![Arc::clone(&max_u64)],
            0,
            num_rows,
            all_null.clone(),
        ),
        // LEAD with 2^64 - 1 (default = -1)
        build_window_tester_with_args(
            ast::WindowFuncLead,
            TypeLonglong,
            vec![Arc::clone(&max_u64), Arc::clone(&def_minus_one)],
            0,
            num_rows,
            all_def.clone(),
        ),
        // LEAD with 2^64 - 2 (no default): rows 0..1 stay out-of-range, rows 2..4 would wrap to 0..2 in Go
        build_window_tester_with_args(
            ast::WindowFuncLead,
            TypeLonglong,
            vec![Arc::clone(&max_u64_m1)],
            0,
            num_rows,
            all_null.clone(),
        ),
        // LEAD with 2^64 - 2 (default = -1)
        build_window_tester_with_args(
            ast::WindowFuncLead,
            TypeLonglong,
            vec![Arc::clone(&max_u64_m1), Arc::clone(&def_minus_one)],
            0,
            num_rows,
            all_def.clone(),
        ),
        // Control: LEAD with 2^63 - 1 (large offset that does not wrap in Go either)
        build_window_tester_with_args(
            ast::WindowFuncLead,
            TypeLonglong,
            vec![Arc::clone(&max_i64)],
            0,
            num_rows,
            all_null.clone(),
        ),
        build_window_tester_with_args(
            ast::WindowFuncLead,
            TypeLonglong,
            vec![Arc::clone(&max_i64), Arc::clone(&def_minus_one)],
            0,
            num_rows,
            all_def.clone(),
        ),
        // Control: LAG with 2^64 - 1 and 2^64 - 2 (unchanged comparison logic)
        build_window_tester_with_args(
            ast::WindowFuncLag,
            TypeLonglong,
            vec![max_u64],
            0,
            num_rows,
            all_null,
        ),
        build_window_tester_with_args(
            ast::WindowFuncLag,
            TypeLonglong,
            vec![max_u64_m1, def_minus_one],
            0,
            num_rows,
            all_def,
        ),
    ];

    for t in &tests {
        run_window_test(t);
    }
}

#[test]
fn test_window_functions() {
    let i = new_int_datum;
    let f = new_float64_datum;
    let s = new_string_datum;

    let tests = vec![
        build_window_tester(ast::WindowFuncCumeDist, TypeLonglong, 0, 1, 1, vec![f(1.0)]),
        build_window_tester(
            ast::WindowFuncCumeDist,
            TypeLonglong,
            0,
            0,
            2,
            vec![f(1.0), f(1.0)],
        ),
        build_window_tester(
            ast::WindowFuncCumeDist,
            TypeLonglong,
            0,
            1,
            4,
            vec![f(0.25), f(0.5), f(0.75), f(1.0)],
        ),
        build_window_tester(
            ast::WindowFuncDenseRank,
            TypeLonglong,
            0,
            0,
            2,
            vec![i(1), i(1)],
        ),
        build_window_tester(
            ast::WindowFuncDenseRank,
            TypeLonglong,
            0,
            1,
            4,
            vec![i(1), i(2), i(3), i(4)],
        ),
        build_window_tester(
            ast::WindowFuncFirstValue,
            TypeLonglong,
            0,
            1,
            2,
            vec![i(0), i(0)],
        ),
        build_window_tester(
            ast::WindowFuncFirstValue,
            TypeFloat,
            0,
            1,
            2,
            vec![f(0.0), f(0.0)],
        ),
        build_window_tester(
            ast::WindowFuncFirstValue,
            TypeDouble,
            0,
            1,
            2,
            vec![f(0.0), f(0.0)],
        ),
        build_window_tester(
            ast::WindowFuncFirstValue,
            TypeNewDecimal,
            0,
            1,
            2,
            vec![dec(0), dec(0)],
        ),
        build_window_tester(
            ast::WindowFuncFirstValue,
            TypeString,
            0,
            1,
            2,
            vec![s("0"), s("0")],
        ),
        build_window_tester(
            ast::WindowFuncFirstValue,
            TypeDate,
            0,
            1,
            2,
            vec![date(365), date(365)],
        ),
        build_window_tester(
            ast::WindowFuncFirstValue,
            TypeDuration,
            0,
            1,
            2,
            vec![
                tidb_types::datum::new_duration_datum(dur(0)),
                tidb_types::datum::new_duration_datum(dur(0)),
            ],
        ),
        build_window_tester(
            ast::WindowFuncFirstValue,
            TypeJSON,
            0,
            1,
            2,
            vec![json(0), json(0)],
        ),
        build_window_tester(
            ast::WindowFuncLastValue,
            TypeLonglong,
            1,
            0,
            2,
            vec![i(1), i(1)],
        ),
        build_window_tester(
            ast::WindowFuncNthValue,
            TypeLonglong,
            2,
            0,
            3,
            vec![i(1), i(1), i(1)],
        ),
        build_window_tester(
            ast::WindowFuncNthValue,
            TypeLonglong,
            5,
            0,
            3,
            vec![null(), null(), null()],
        ),
        build_window_tester(
            ast::WindowFuncNtile,
            TypeLonglong,
            3,
            0,
            4,
            vec![i(1), i(1), i(2), i(3)],
        ),
        build_window_tester(
            ast::WindowFuncNtile,
            TypeLonglong,
            5,
            0,
            3,
            vec![i(1), i(2), i(3)],
        ),
        build_window_tester(
            ast::WindowFuncPercentRank,
            TypeLonglong,
            0,
            1,
            1,
            vec![f(0.0)],
        ),
        build_window_tester(
            ast::WindowFuncPercentRank,
            TypeLonglong,
            0,
            0,
            3,
            vec![f(0.0), f(0.0), f(0.0)],
        ),
        build_window_tester(
            ast::WindowFuncPercentRank,
            TypeLonglong,
            0,
            1,
            4,
            vec![f(0.0), f(0.3333333333333333), f(0.6666666666666666), f(1.0)],
        ),
        build_window_tester(ast::WindowFuncRank, TypeLonglong, 0, 1, 1, vec![i(1)]),
        build_window_tester(
            ast::WindowFuncRank,
            TypeLonglong,
            0,
            0,
            3,
            vec![i(1), i(1), i(1)],
        ),
        build_window_tester(
            ast::WindowFuncRank,
            TypeLonglong,
            0,
            1,
            4,
            vec![i(1), i(2), i(3), i(4)],
        ),
        build_window_tester(
            ast::WindowFuncRowNumber,
            TypeLonglong,
            0,
            0,
            4,
            vec![i(1), i(2), i(3), i(4)],
        ),
    ];

    for t in &tests {
        run_window_test(t);
    }
}

#[test]
fn test_deque_reset() {
    let mut deque = new_deque(true, |i: &i64, j: &i64| i.cmp(j) as isize);
    deque.push_back(0, 12);
    deque.reset();
    assert_eq!(deque.items().len(), 0);
    assert!(deque.is_max);
}

#[test]
fn test_deque_push_pop() {
    let mut deque = new_deque(true, |i: &i64, j: &i64| i.cmp(j) as isize);
    let times = 15usize;
    for i in 0..times {
        if i != 0 {
            let front = deque.front().expect("non-empty front");
            assert_eq!(front.item, 0);
            assert_eq!(front.idx, 0);
        }
        deque.push_back(i as u64, i as i64);
        let back = deque.back().expect("non-empty back");
        assert_eq!(back.item, i as i64);
        assert_eq!(back.idx, i as u64);
    }
    for i in 0..times {
        let pair = deque.back().expect("non-empty back");
        assert_eq!(pair.item, (times - i - 1) as i64);
        assert_eq!(pair.idx, (times - i - 1) as u64);
        let front = deque.front().expect("non-empty front");
        assert_eq!(front.item, 0);
        assert_eq!(front.idx, 0);
        deque.pop_back().unwrap();
    }
    assert!(deque.is_empty());
    assert!(deque.pop_back().is_err());
    assert!(deque.pop_front().is_err());
}

/// Verifies that all sliding-capable window functions expose `as_sliding_window()`,
/// `as_max_min_sliding_mut()` where applicable, and slide accurately over a moving frame.
#[test]
fn test_sliding_window_functions() {
    let ctx = es::new_expr_context(vec![]);
    let ec = ctx.get_eval_ctx();
    let ec: &dyn EvalContext = &*ec;
    let st = EvalState::new();

    // Sliding MAX over int64 [10, 30, 20, NULL, 5] with window size 2:
    // [0..2) -> 30, [1..3) -> 30, [2..4) -> 20, [3..5) -> 5
    let ft = new_field_type(TypeLonglong);
    let arg = Expr::Column(Column {
        ret_type: Arc::new(ft.clone()),
        index: 0,
        ..Column::default()
    })
    .into_ref();
    let desc = new_agg_func_desc(&*ctx, ast::AggFuncMax, vec![arg], false).unwrap();
    let mut func = build_window_functions(&*ctx, &desc, 0, &[])
        .unwrap()
        .unwrap();
    assert!(func.as_sliding_window().is_some());
    assert!(func.as_max_min_sliding_mut().is_some());

    let mut chk = new_chunk_with_capacity(std::slice::from_ref(&ft), 5);
    chk.append_int64(0, 10);
    chk.append_int64(0, 30);
    chk.append_int64(0, 20);
    chk.append_null(0);
    chk.append_int64(0, 5);
    let get_row = |idx: u64| chk.get_row(idx as usize);

    func.as_max_min_sliding_mut().unwrap().set_window_start(0);
    let mut pr = func.alloc_partial_result();
    let init_rows = vec![chk.get_row(0), chk.get_row(1)];
    func.update_partial_result(ec, &st, &init_rows, &mut pr)
        .unwrap();

    let mut out = new_chunk_with_capacity(std::slice::from_ref(&ft), 4);
    func.append_final_result2_chunk(ec, &mut pr, &mut out)
        .unwrap();
    let sw = func.as_sliding_window().unwrap();
    sw.slide(ec, &st, &get_row, 0, 2, 1, 1, &mut pr).unwrap();
    func.append_final_result2_chunk(ec, &mut pr, &mut out)
        .unwrap();
    sw.slide(ec, &st, &get_row, 1, 3, 1, 1, &mut pr).unwrap();
    func.append_final_result2_chunk(ec, &mut pr, &mut out)
        .unwrap();
    sw.slide(ec, &st, &get_row, 2, 4, 1, 1, &mut pr).unwrap();
    func.append_final_result2_chunk(ec, &mut pr, &mut out)
        .unwrap();

    assert_eq!(out.get_row(0).get_int64(0), 30);
    assert_eq!(out.get_row(1).get_int64(0), 30);
    assert_eq!(out.get_row(2).get_int64(0), 20);
    assert_eq!(out.get_row(3).get_int64(0), 5);

    // High-precision SUM / AVG must NOT implement SlidingWindowAggFunc.
    let hp_ctx = es::new_expr_context(vec![es::with_windowing_use_high_precision(true)]);
    let lp_ctx = es::new_expr_context(vec![es::with_windowing_use_high_precision(false)]);
    let dbl_ft = new_field_type(TypeDouble);
    let dbl_arg = Expr::Column(Column {
        ret_type: Arc::new(dbl_ft),
        index: 0,
        ..Column::default()
    })
    .into_ref();
    for name in [ast::AggFuncSum, ast::AggFuncAvg] {
        let d = new_agg_func_desc(&*hp_ctx, name, vec![Arc::clone(&dbl_arg)], false).unwrap();
        let f_hp = build_window_functions(&*hp_ctx, &d, 0, &[])
            .unwrap()
            .unwrap();
        assert!(f_hp.as_sliding_window().is_none(), "{name} high-precision");

        let f_lp = build_window_functions(&*lp_ctx, &d, 0, &[])
            .unwrap()
            .unwrap();
        assert!(f_lp.as_sliding_window().is_some(), "{name} low-precision");
    }

    // BIT_XOR slides; BIT_OR and BIT_AND do not.
    let int_arg = Expr::Column(Column {
        ret_type: Arc::new(ft.clone()),
        index: 0,
        ..Column::default()
    })
    .into_ref();
    for (name, expected_sliding) in [
        (ast::AggFuncBitXor, true),
        (ast::AggFuncBitOr, false),
        (ast::AggFuncBitAnd, false),
    ] {
        let d = new_agg_func_desc(&*ctx, name, vec![Arc::clone(&int_arg)], false).unwrap();
        let f = build_window_functions(&*ctx, &d, 0, &[]).unwrap().unwrap();
        assert_eq!(f.as_sliding_window().is_some(), expected_sliding, "{name}");
    }
}

/// Memory-accounting property tests corresponding to Go's `TestMemPercentRank`,
/// `TestMemCumeDist`, `TestMemNtile`, `TestMemRank`, `TestMemRowNumber`, `TestMemValue`,
/// `TestMemLeadLag`, and sliding `MAX`/`MIN` (tracker grows on update and returns to 0 on
/// drop).
#[test]
fn test_window_memory_accounting_properties() {
    let ctx = es::new_expr_context(vec![]);
    let ec = ctx.get_eval_ctx();
    let ec: &dyn EvalContext = &*ec;
    let st = EvalState::new();

    // 1. Row-storing window functions: RANK, DENSE_RANK, PERCENT_RANK, CUME_DIST, LEAD, LAG
    let str_ft = new_field_type(TypeString);
    let col0 = Column {
        ret_type: Arc::new(str_ft.clone()),
        index: 0,
        ..Column::default()
    };
    let col0_expr = Expr::Column(col0.clone()).into_ref();
    let mut chk = new_chunk_with_capacity(std::slice::from_ref(&str_ft), 32);
    for i in 0..32 {
        chk.append_string(0, format!("payload-{i:04}-{}", "x".repeat(64)).as_bytes());
    }
    let borrowed_rows: Vec<Row<'_>> = (0..chk.num_rows()).map(|i| chk.get_row(i)).collect();

    for name in [
        ast::WindowFuncRank,
        ast::WindowFuncDenseRank,
        ast::WindowFuncPercentRank,
        ast::WindowFuncCumeDist,
        ast::WindowFuncLead,
        ast::WindowFuncLag,
    ] {
        let tracker = new_tracker(1, -1);
        {
            let tracked = TrackedAlloc::new(Arc::clone(&tracker));
            let alloc = ColAlloc::Tracked(tracked.clone());
            let args = if name == ast::WindowFuncLead || name == ast::WindowFuncLag {
                vec![Arc::clone(&col0_expr), Expr::Constant(new_one()).into_ref()]
            } else {
                vec![]
            };
            let desc = new_agg_func_desc(&*ctx, name, args, false).unwrap();
            let mut f = build_window_functions(&*ctx, &desc, 0, std::slice::from_ref(&col0))
                .unwrap()
                .unwrap();
            f.set_alloc(alloc);

            let mut pr = f.alloc_partial_result();
            f.update_partial_result(ec, &st, &borrowed_rows, &mut pr)
                .unwrap();
            tracked.flush();
            assert!(
                tracker.bytes_consumed() > 0,
                "{name}: expected tracker > 0 after updating rows"
            );
            // Zero-copy RowHandle ingestion path also works and grows the handle vector.
            let arc_chk = Arc::new(chk.clone());
            let handles: Vec<RowHandle> = (0..4)
                .map(|i| RowHandle::new(Arc::clone(&arc_chk), i))
                .collect();
            f.update_partial_result_handles(ec, &st, &handles, &mut pr)
                .unwrap();

            let mut out = new_chunk_with_capacity(std::slice::from_ref(&*desc.base.ret_tp), 1);
            f.append_final_result2_chunk(ec, &mut pr, &mut out).unwrap();
            f.reset_partial_result(&mut pr);
            drop(pr);
            tracked.flush();
        }
        assert_eq!(
            tracker.bytes_consumed(),
            0,
            "{name}: expected tracker == 0 after drop"
        );
    }

    // 2. Variable-length value evaluators: FIRST_VALUE, LAST_VALUE, NTH_VALUE across String,
    // JSON, and VectorFloat32.
    for (tp, name) in [
        (TypeString, ast::WindowFuncFirstValue),
        (TypeString, ast::WindowFuncLastValue),
        (TypeString, ast::WindowFuncNthValue),
        (TypeJSON, ast::WindowFuncFirstValue),
        (TypeJSON, ast::WindowFuncLastValue),
        (TypeTiDBVectorFloat32, ast::WindowFuncFirstValue),
        (TypeTiDBVectorFloat32, ast::WindowFuncLastValue),
    ] {
        let tracker = new_tracker(2, -1);
        {
            let tracked = TrackedAlloc::new(Arc::clone(&tracker));
            let alloc = ColAlloc::Tracked(tracked.clone());
            let ft = new_field_type(tp);
            let mut src = new_chunk_with_capacity(std::slice::from_ref(&ft), 2);
            match tp {
                TypeString => {
                    src.append_string(0, &"a".repeat(256).into_bytes());
                    src.append_string(0, &"b".repeat(512).into_bytes());
                }
                TypeJSON => {
                    let j1 = create_binary_json(JSONValue::String("a".repeat(256).into_bytes()));
                    let j2 = create_binary_json(JSONValue::String("b".repeat(512).into_bytes()));
                    src.append_json(0, &j1);
                    src.append_json(0, &j2);
                }
                TypeTiDBVectorFloat32 => {
                    let v1 = must_create_vector_float32(&vec![1.5f32; 64]);
                    let v2 = must_create_vector_float32(&vec![2.5f32; 128]);
                    src.append_vector_float32(0, &v1);
                    src.append_vector_float32(0, &v2);
                }
                _ => unreachable!(),
            }
            let arg0 = Expr::Column(Column {
                ret_type: Arc::new(ft.clone()),
                index: 0,
                ..Column::default()
            })
            .into_ref();
            let mut args = vec![arg0];
            if name == ast::WindowFuncNthValue {
                args.push(
                    Expr::Constant(Constant::new(
                        new_uint_datum(2),
                        new_field_type(TypeLonglong),
                    ))
                    .into_ref(),
                );
            }
            let desc = if tp == TypeTiDBVectorFloat32 {
                // Construct directly so VectorFloat32 is tested regardless of desc inference.
                let base =
                    BaseFuncDesc::from_parts(name.to_owned(), args, Some(Arc::new(ft.clone())));
                AggFuncDesc::from_base(base, false)
            } else {
                new_agg_func_desc(&*ctx, name, args, false).unwrap()
            };
            let mut f = build_window_functions(&*ctx, &desc, 0, &[])
                .unwrap()
                .unwrap();
            f.set_alloc(alloc);
            let mut pr = f.alloc_partial_result();
            let rs = vec![src.get_row(0), src.get_row(1)];
            f.update_partial_result(ec, &st, &rs, &mut pr).unwrap();
            tracked.flush();
            assert!(
                tracker.bytes_consumed() >= 256,
                "{name} tp={tp}: expected payload bytes tracked, got {}",
                tracker.bytes_consumed()
            );
            let mut out = new_chunk_with_capacity(std::slice::from_ref(&ft), 1);
            f.append_final_result2_chunk(ec, &mut pr, &mut out).unwrap();
            assert!(!out.get_row(0).is_null(0));
            drop(pr);
            tracked.flush();
        }
        assert_eq!(
            tracker.bytes_consumed(),
            0,
            "{name} tp={tp}: expected tracker == 0 after drop"
        );
    }

    // 3. Sliding MAX/MIN string deque tracks both deque buffer and string element bytes.
    {
        let tracker = new_tracker(3, -1);
        {
            let tracked = TrackedAlloc::new(Arc::clone(&tracker));
            let alloc = ColAlloc::Tracked(tracked.clone());
            let desc =
                new_agg_func_desc(&*ctx, ast::AggFuncMax, vec![Arc::clone(&col0_expr)], false)
                    .unwrap();
            let mut f = build_window_functions(&*ctx, &desc, 0, &[])
                .unwrap()
                .unwrap();
            f.set_alloc(alloc);
            let mut pr = f.alloc_partial_result();
            f.update_partial_result(ec, &st, &borrowed_rows, &mut pr)
                .unwrap();
            tracked.flush();
            assert!(tracker.bytes_consumed() > 0);
            f.reset_partial_result(&mut pr);
            drop(pr);
            tracked.flush();
        }
        assert_eq!(tracker.bytes_consumed(), 0);
    }

    // 4. Fixed-size window functions (ROW_NUMBER, NTILE): reset behaves like fresh.
    for (name, args) in [
        (ast::WindowFuncRowNumber, vec![]),
        (
            ast::WindowFuncNtile,
            vec![
                Expr::Constant(Constant::new(
                    new_uint_datum(3),
                    new_field_type(TypeLonglong),
                ))
                .into_ref(),
            ],
        ),
    ] {
        let desc = new_agg_func_desc(&*ctx, name, args, false).unwrap();
        let f = build_window_functions(&*ctx, &desc, 0, &[])
            .unwrap()
            .unwrap();
        let mut pr = f.alloc_partial_result();
        f.update_partial_result(ec, &st, &borrowed_rows[..4], &mut pr)
            .unwrap();
        let mut out1 = new_chunk_with_capacity(std::slice::from_ref(&*desc.base.ret_tp), 4);
        for _ in 0..4 {
            f.append_final_result2_chunk(ec, &mut pr, &mut out1)
                .unwrap();
        }
        f.reset_partial_result(&mut pr);
        f.update_partial_result(ec, &st, &borrowed_rows[..4], &mut pr)
            .unwrap();
        let mut out2 = new_chunk_with_capacity(std::slice::from_ref(&*desc.base.ret_tp), 4);
        for _ in 0..4 {
            f.append_final_result2_chunk(ec, &mut pr, &mut out2)
                .unwrap();
        }
        for r in 0..4 {
            assert_eq!(out1.get_row(r).get_raw(0), out2.get_row(r).get_raw(0));
        }
    }
}

/// Static assertions for `build_window_functions` and the `Send + Sync` / `Send` bounds.
#[test]
fn test_window_funcs_send_sync_and_unknown_fallback() {
    let ctx = es::new_expr_context(vec![]);
    let desc = AggFuncDesc::from_base(
        BaseFuncDesc::from_parts(
            "nonexistent_window_func".to_owned(),
            vec![],
            Some(Arc::new(new_field_type(TypeLonglong))),
        ),
        false,
    );
    assert!(
        build_window_functions(&*ctx, &desc, 0, &[])
            .unwrap()
            .is_none()
    );

    // VectorFloat32 with NTH_VALUE(0) and NTILE(0) returns NULL.
    let ntile0 = new_agg_func_desc(
        &*ctx,
        ast::WindowFuncNtile,
        vec![Expr::Constant(new_zero()).into_ref()],
        false,
    )
    .unwrap();
    let f = build_window_functions(&*ctx, &ntile0, 0, &[])
        .unwrap()
        .unwrap();
    let mut pr = f.alloc_partial_result();
    let ec = ctx.get_eval_ctx();
    let mut out = new_chunk_with_capacity(std::slice::from_ref(&*ntile0.base.ret_tp), 1);
    f.append_final_result2_chunk(&*ec, &mut pr, &mut out)
        .unwrap();
    assert!(out.get_row(0).is_null(0));

    // Duration / Time sliding MAX check with non-zero window_start.
    let dur_ft = new_field_type(TypeDuration);
    let dur_arg = Expr::Column(Column {
        ret_type: Arc::new(dur_ft.clone()),
        index: 0,
        ..Column::default()
    })
    .into_ref();
    let d = new_agg_func_desc(&*ctx, ast::AggFuncMin, vec![dur_arg], false).unwrap();
    let mut f_dur = build_window_functions(&*ctx, &d, 0, &[]).unwrap().unwrap();
    f_dur.as_max_min_sliding_mut().unwrap().set_window_start(2);
    let mut dur_chk = new_chunk_with_capacity(std::slice::from_ref(&dur_ft), 4);
    for ns in [500, 100, 300, 200] {
        dur_chk.append_duration(
            0,
            Duration {
                duration: ns,
                fsp: 0,
            },
        );
    }
    let mut pr = f_dur.alloc_partial_result();
    let st = EvalState::new();
    // Initial frame [2..4): {300, 200} -> min 200.
    f_dur
        .update_partial_result(
            &*ec,
            &st,
            &[dur_chk.get_row(2), dur_chk.get_row(3)],
            &mut pr,
        )
        .unwrap();
    let mut out = new_chunk_with_capacity(std::slice::from_ref(&dur_ft), 1);
    f_dur
        .append_final_result2_chunk(&*ec, &mut pr, &mut out)
        .unwrap();
    assert_eq!(out.get_row(0).get_duration(0, 0).duration, 200);
    // Slide from [2..4) to [3..4) (shift_start=1, shift_end=0): {200} -> min 200.
    let get_row = |idx: u64| dur_chk.get_row(idx as usize);
    f_dur
        .as_sliding_window()
        .unwrap()
        .slide(&*ec, &st, &get_row, 2, 4, 1, 0, &mut pr)
        .unwrap();
    out.reset();
    f_dur
        .append_final_result2_chunk(&*ec, &mut pr, &mut out)
        .unwrap();
    assert_eq!(out.get_row(0).get_duration(0, 0).duration, 200);
    // Empty frame [4..4) (shift_start=1, shift_end=0): NULL.
    f_dur
        .as_sliding_window()
        .unwrap()
        .slide(&*ec, &st, &get_row, 3, 4, 1, 0, &mut pr)
        .unwrap();
    out.reset();
    f_dur
        .append_final_result2_chunk(&*ec, &mut pr, &mut out)
        .unwrap();
    assert!(out.get_row(0).is_null(0));

    // Also cover Time sliding with time_from_days.
    let time_ft = new_field_type(TypeDate);
    let time_arg = Expr::Column(Column {
        ret_type: Arc::new(time_ft.clone()),
        index: 0,
        ..Column::default()
    })
    .into_ref();
    let td = new_agg_func_desc(&*ctx, ast::AggFuncMax, vec![time_arg], false).unwrap();
    let f_time = build_window_functions(&*ctx, &td, 0, &[]).unwrap().unwrap();
    let mut time_chk = new_chunk_with_capacity(std::slice::from_ref(&time_ft), 3);
    time_chk.append_time(0, time_from_days(400));
    time_chk.append_time(0, time_from_days(390));
    time_chk.append_time(0, time_from_days(395));
    let mut pr = f_time.alloc_partial_result();
    f_time
        .update_partial_result(
            &*ec,
            &st,
            &[time_chk.get_row(0), time_chk.get_row(1)],
            &mut pr,
        )
        .unwrap();
    let get_t = |idx: u64| time_chk.get_row(idx as usize);
    f_time
        .as_sliding_window()
        .unwrap()
        .slide(&*ec, &st, &get_t, 0, 2, 1, 1, &mut pr)
        .unwrap();
    let mut out = new_chunk_with_capacity(std::slice::from_ref(&time_ft), 1);
    f_time
        .append_final_result2_chunk(&*ec, &mut pr, &mut out)
        .unwrap();
    assert_eq!(out.get_row(0).get_time(0), time_from_days(395));

    // Also verify new_dec_from_int and mysql constants compile cleanly.
    let _ = new_dec_from_int(0);
    let _ = mysql::TypeLonglong;
}
