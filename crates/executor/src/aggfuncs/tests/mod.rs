//! The unit tests of Go's `aggfuncs` for the functions of part A (spec D12.4 §6, the core
//! aggregate functions): `TestCount`, `TestSum`, `TestAvg`, `TestMaxMin` and
//! `TestMergePartialResult4{Count,Sum,Avg,FirstRow,MaxMin,BitFuncs}` with Go's helpers
//! `testAggFunc`, `testMultiArgsAggFunc` and `testMergePartialResult` (aggfunc_test.go), and
//! the `TestMem*` tests as properties of the Rust memory accounting.
//!
//! Differences from the Go tests:
//! - Go's `messUpChunk` overwrites the source chunk after each update, to catch partial
//!   results that keep a reference into it. A Rust partial result cannot borrow the chunk
//!   (`PartialResult` is `'static`), so the helpers drop it instead.
//! - COUNT(DISTINCT) is part B1 of the block: where Go's helpers build it, the Rust helpers
//!   assert the temporary `blocked` error. The APPROX_COUNT_DISTINCT cases (part B2) are left
//!   out.
//! - `TestMem*` check Go's estimated `memDelta`, which Rust does not return (the memory
//!   exception A1): the DISTINCT sets grow a memory tracker through the allocator and give it
//!   back when dropped, and every function's reset partial result behaves as a fresh one.

// Go identifiers for the type constants in patterns (M-GONAME).
#![allow(non_upper_case_globals)]

use std::sync::Arc;

use tidb_base::Error;
use tidb_base::blocked::blocked_by;
use tidb_base::mysql::{
    self, TypeDate, TypeDouble, TypeDuration, TypeEnum, TypeFloat, TypeJSON, TypeLonglong,
    TypeNewDecimal, TypeSet, TypeString, UnsignedFlag,
};
use tidb_chunk::ColAlloc;
use tidb_chunk::chunk::{Chunk, new_chunk_with_capacity};
use tidb_chunk::row::Row;
use tidb_expression::aggregation::{AggFuncDesc, AggFunctionMode, new_agg_func_desc};
use tidb_expression::ast_funcs as ast;
use tidb_expression::context::{BuildContext, EvalContext, type_ctx};
use tidb_expression::expression::EvalState;
use tidb_expression::{Column, Expr, ExprRef};
use tidb_memory::TrackedAlloc;
use tidb_memory::tracker::new_tracker;
use tidb_parser::types::FieldType;
use tidb_sessionctx::exprstatic as es;
use tidb_types::collate::get_collator;
use tidb_types::datum::{
    Datum, new_collate_mysql_enum_datum, new_decimal_datum, new_duration_datum, new_float32_datum,
    new_float64_datum, new_int_datum, new_json_datum, new_mysql_set_datum, new_string_datum,
    new_time_datum,
};
use tidb_types::enum_type::{parse_enum, parse_enum_name, parse_enum_value};
use tidb_types::field_type::new_field_type;
use tidb_types::json::{JSONValue, create_binary_json};
use tidb_types::mydecimal::new_dec_from_int;
use tidb_types::set_type::{parse_set, parse_set_name, parse_set_value};
use tidb_types::time::{Duration, time_from_days};

use super::{
    AggFunc, PartialResult, build, new_agg_partial_result_mapper,
    new_agg_partial_result_mapper_with_cap,
};

// aggTest (aggfunc_test.go).
struct AggTest {
    data_type: FieldType,
    num_rows: usize,
    func_name: &'static str,
    results: Vec<Datum>,
}

// multiArgsAggTest (aggfunc_test.go).
struct MultiArgsAggTest {
    data_types: Vec<FieldType>,
    num_rows: usize,
    func_name: &'static str,
    results: Vec<Datum>,
}

fn elems() -> Vec<Vec<u8>> {
    ["e", "d", "c", "b", "a"]
        .iter()
        .map(|s| s.as_bytes().to_vec())
        .collect()
}

// getDataGenFunc (aggfunc_test.go).
fn data_gen(ft: &FieldType, i: usize) -> Datum {
    let i = i as i64;
    match ft.get_type() {
        TypeLonglong => new_int_datum(i),
        TypeFloat => new_float32_datum(i as f32),
        TypeNewDecimal => new_decimal_datum(new_dec_from_int(i)),
        TypeDouble => new_float64_datum(i as f64),
        TypeString => new_string_datum(i.to_string()),
        TypeDate => new_time_datum(time_from_days(i + 365)),
        TypeDuration => new_duration_datum(dur(i)),
        TypeJSON => json(i),
        TypeEnum => {
            let e = parse_enum_value(&elems(), (i + 1) as u64).unwrap();
            new_collate_mysql_enum_datum(e, ft.get_collate())
        }
        TypeSet => {
            let e = parse_set_value(&elems(), (i + 1) as u64).unwrap();
            new_mysql_set_datum(e, ft.get_collate())
        }
        tp => panic!("no data generator for type {tp}"),
    }
}

fn dur(i: i64) -> Duration {
    Duration {
        duration: i,
        fsp: 0,
    }
}

fn json(i: i64) -> Datum {
    new_json_datum(create_binary_json(JSONValue::Int64(i)))
}

fn dec(i: i64) -> Datum {
    new_decimal_datum(new_dec_from_int(i))
}

fn date(days: i64) -> Datum {
    new_time_datum(time_from_days(days))
}

fn null() -> Datum {
    Datum::default()
}

// buildAggTester (aggfunc_test.go).
fn agg_tester(func_name: &'static str, tp: u8, num_rows: usize, results: Vec<Datum>) -> AggTest {
    agg_tester_with_field_type(func_name, new_field_type(tp), num_rows, results)
}

// buildAggTesterWithFieldType (aggfunc_test.go).
fn agg_tester_with_field_type(
    func_name: &'static str,
    ft: FieldType,
    num_rows: usize,
    results: Vec<Datum>,
) -> AggTest {
    AggTest {
        data_type: ft,
        num_rows,
        func_name,
        results,
    }
}

// buildMultiArgsAggTester (aggfunc_test.go).
fn multi_args_agg_tester(
    func_name: &'static str,
    tps: &[u8],
    num_rows: usize,
    results: Vec<Datum>,
) -> MultiArgsAggTest {
    MultiArgsAggTest {
        data_types: tps.iter().map(|&tp| new_field_type(tp)).collect(),
        num_rows,
        func_name,
        results,
    }
}

// aggTest.genSrcChk (aggfunc_test.go): the rows 0..numRows and a NULL.
fn gen_src_chk(p: &AggTest) -> Chunk {
    let mut chk = new_chunk_with_capacity(std::slice::from_ref(&p.data_type), p.num_rows);
    for i in 0..p.num_rows {
        chk.append_datum(0, &data_gen(&p.data_type, i));
    }
    chk.append_datum(0, &null());
    chk
}

// multiArgsAggTest.genSrcChk (aggfunc_test.go): as Go, only the first column gets the NULL.
fn gen_multi_src_chk(p: &MultiArgsAggTest) -> Chunk {
    let mut chk = new_chunk_with_capacity(&p.data_types, p.num_rows);
    for i in 0..p.num_rows {
        for (j, ft) in p.data_types.iter().enumerate() {
            chk.append_datum(j, &data_gen(ft, i));
        }
    }
    chk.append_datum(0, &null());
    chk
}

fn rows(chk: &Chunk) -> Vec<Row<'_>> {
    (0..chk.num_rows()).map(|i| chk.get_row(i)).collect()
}

fn column(ft: &FieldType, index: isize) -> ExprRef {
    Expr::Column(Column {
        ret_type: Arc::new(ft.clone()),
        index,
        ..Column::default()
    })
    .into_ref()
}

fn built(ctx: &es::ExprContext, desc: &AggFuncDesc) -> Box<dyn AggFunc> {
    build(ctx, desc, 0)
        .expect("the function of part A builds")
        .expect("the descriptor has a function")
}

// Asserts the temporary error of a function of a later part of the block.
fn assert_blocked(res: Result<Option<Box<dyn AggFunc>>, Error>) {
    match res {
        Err(e) => assert_eq!(blocked_by(&e), Some("D12.4"), "{e:?}"),
        Ok(_) => panic!("a function of a later part of the block was built"),
    }
}

fn update_each(
    ec: &dyn EvalContext,
    f: &dyn AggFunc,
    rows: &[Row<'_>],
    pr: &mut PartialResult,
) -> Result<(), Error> {
    let st = EvalState::new();
    for row in rows {
        f.update_partial_result(ec, &st, std::slice::from_ref(row), pr)?;
    }
    Ok(())
}

fn final_datum(
    ec: &dyn EvalContext,
    f: &dyn AggFunc,
    pr: &mut PartialResult,
    ft: &FieldType,
) -> Datum {
    let mut chk = new_chunk_with_capacity(std::slice::from_ref(ft), 1);
    f.append_final_result2_chunk(ec, pr, &mut chk).unwrap();
    chk.get_row(0).get_datum(0, ft)
}

fn assert_datum_eq(ec: &dyn EvalContext, got: &Datum, want: &Datum, collate: &str) {
    let (cmp, err) = got.compare(&type_ctx(ec), want, get_collator(collate));
    err.unwrap();
    assert_eq!(cmp, 0, "{got:?} != {want:?}");
}

// testAggFunc (aggfunc_test.go).
fn test_agg_func(p: &AggTest) {
    let ctx = es::new_expr_context(vec![]);
    let ec = ctx.get_eval_ctx();
    let ec: &dyn EvalContext = &*ec;
    let args = vec![column(&p.data_type, 0)];
    let collate = p.data_type.get_collate();

    let desc = new_agg_func_desc(&*ctx, p.func_name, args.clone(), false).unwrap();
    let final_func = built(&ctx, &desc);
    let mut final_pr = final_func.alloc_partial_result();
    let src = gen_src_chk(p);
    update_each(ec, &*final_func, &rows(&src), &mut final_pr).unwrap();
    drop(src);
    let dt = final_datum(ec, &*final_func, &mut final_pr, &desc.ret_tp);
    assert_datum_eq(ec, &dt, &p.results[1], collate);

    // test the empty input
    final_func.reset_partial_result(&mut final_pr);
    let dt = final_datum(ec, &*final_func, &mut final_pr, &desc.ret_tp);
    assert_datum_eq(ec, &dt, &p.results[0], collate);

    // test the agg func with distinct
    let desc = new_agg_func_desc(&*ctx, p.func_name, args, true).unwrap();
    if p.func_name == ast::AggFuncCount {
        assert_blocked(build(&*ctx, &desc, 0));
        return;
    }
    let final_func = built(&ctx, &desc);
    let mut final_pr = final_func.alloc_partial_result();
    for _ in 0..2 {
        let src = gen_src_chk(p);
        update_each(ec, &*final_func, &rows(&src), &mut final_pr).unwrap();
    }
    let dt = final_datum(ec, &*final_func, &mut final_pr, &desc.ret_tp);
    assert_datum_eq(ec, &dt, &p.results[1], collate);
}

// testMultiArgsAggFunc (aggfunc_test.go).
fn test_multi_args_agg_func(p: &MultiArgsAggTest) {
    let ctx = es::new_expr_context(vec![]);
    let ec = ctx.get_eval_ctx();
    let ec: &dyn EvalContext = &*ec;
    let args: Vec<ExprRef> = p
        .data_types
        .iter()
        .enumerate()
        .map(|(k, ft)| column(ft, k as isize))
        .collect();

    let desc = new_agg_func_desc(&*ctx, p.func_name, args.clone(), false).unwrap();
    let collate = args[0].get_type(ec).get_collate();
    let final_func = built(&ctx, &desc);
    let mut final_pr = final_func.alloc_partial_result();
    let src = gen_multi_src_chk(p);
    // Go ignores the update errors (its FIXME); COUNT has none.
    update_each(ec, &*final_func, &rows(&src), &mut final_pr).unwrap();
    let dt = final_datum(ec, &*final_func, &mut final_pr, &desc.ret_tp);
    assert_datum_eq(ec, &dt, &p.results[1], collate);

    // test the empty input
    final_func.reset_partial_result(&mut final_pr);
    let dt = final_datum(ec, &*final_func, &mut final_pr, &desc.ret_tp);
    assert_datum_eq(ec, &dt, &p.results[0], collate);

    // test the agg func with distinct: COUNT(DISTINCT) is part B1.
    let desc = new_agg_func_desc(&*ctx, p.func_name, args, true).unwrap();
    assert_eq!(p.func_name, ast::AggFuncCount);
    assert_blocked(build(&*ctx, &desc, 0));
}

// testMergePartialResult (aggfunc_test.go).
fn test_merge_partial_result(p: &AggTest) {
    let ctx = es::new_expr_context(vec![]);
    let ec = ctx.get_eval_ctx();
    let ec: &dyn EvalContext = &*ec;
    let args = vec![column(&p.data_type, 0)];
    let collate = p.data_type.get_collate();
    let desc = new_agg_func_desc(&*ctx, p.func_name, args, false).unwrap();
    let (partial_desc, final_desc) = desc.split(&[0, 1]);

    // build partial func for partial phase.
    let partial_func = built(&ctx, &partial_desc);
    let mut partial_result = partial_func.alloc_partial_result();

    // build final func for final phase.
    let final_func = built(&ctx, &final_desc);
    let mut final_pr = final_func.alloc_partial_result();

    // update partial result.
    let src = gen_src_chk(p);
    update_each(ec, &*partial_func, &rows(&src), &mut partial_result).unwrap();
    drop(src);
    let dt = final_datum(ec, &*partial_func, &mut partial_result, &p.data_type);
    assert_datum_eq(ec, &dt, &p.results[0], collate);

    final_func
        .merge_partial_result(ec, &mut partial_result, &mut final_pr)
        .unwrap();
    partial_func.reset_partial_result(&mut partial_result);

    let src = gen_src_chk(p);
    update_each(ec, &*partial_func, &rows(&src)[2..], &mut partial_result).unwrap();
    drop(src);
    let dt = final_datum(ec, &*partial_func, &mut partial_result, &p.data_type);
    assert_datum_eq(ec, &dt, &p.results[1], collate);
    final_func
        .merge_partial_result(ec, &mut partial_result, &mut final_pr)
        .unwrap();

    let dt = final_datum(ec, &*final_func, &mut final_pr, &p.data_type);
    assert_datum_eq(ec, &dt, &p.results[2], collate);
}

fn unsigned_type() -> FieldType {
    let mut ft = new_field_type(TypeLonglong);
    ft.add_flag(UnsignedFlag);
    ft
}

const ALL_TYPES: [u8; 8] = [
    TypeLonglong,
    TypeFloat,
    TypeDouble,
    TypeNewDecimal,
    TypeString,
    TypeDate,
    TypeDuration,
    TypeJSON,
];

// TestMergePartialResult4Count (func_count_test.go), without APPROX_COUNT_DISTINCT (part B2).
#[test]
fn merge_partial_result_count() {
    let i = new_int_datum;
    test_merge_partial_result(&agg_tester(
        ast::AggFuncCount,
        TypeLonglong,
        5,
        vec![i(5), i(3), i(8)],
    ));
}

// TestCount (func_count_test.go), without APPROX_COUNT_DISTINCT (part B2).
#[test]
fn count() {
    for tp in ALL_TYPES {
        let results = vec![new_int_datum(0), new_int_datum(5)];
        test_agg_func(&agg_tester(ast::AggFuncCount, tp, 5, results));
    }
    for tp in ALL_TYPES {
        let results = vec![new_int_datum(0), new_int_datum(5)];
        test_multi_args_agg_func(&multi_args_agg_tester(
            ast::AggFuncCount,
            &[tp, tp],
            5,
            results,
        ));
    }
}

// TestMergePartialResult4Sum (func_sum_test.go).
#[test]
fn merge_partial_result_sum() {
    let f = new_float64_datum;
    for p in [
        agg_tester(
            ast::AggFuncSum,
            TypeNewDecimal,
            5,
            vec![dec(10), dec(9), dec(19)],
        ),
        agg_tester(
            ast::AggFuncSum,
            TypeDouble,
            5,
            vec![f(10.0), f(9.0), f(19.0)],
        ),
    ] {
        test_merge_partial_result(&p);
    }
}

// TestSum (func_sum_test.go).
#[test]
fn sum() {
    for p in [
        agg_tester(ast::AggFuncSum, TypeNewDecimal, 5, vec![null(), dec(10)]),
        agg_tester(
            ast::AggFuncSum,
            TypeDouble,
            5,
            vec![null(), new_float64_datum(10.0)],
        ),
    ] {
        test_agg_func(&p);
    }
}

// TestMergePartialResult4Avg (func_avg_test.go).
#[test]
fn merge_partial_result_avg() {
    let f = new_float64_datum;
    for tp in [TypeNewDecimal, TypeDouble] {
        test_merge_partial_result(&agg_tester(
            ast::AggFuncAvg,
            tp,
            5,
            vec![f(2.0), f(3.0), f(2.375)],
        ));
    }
}

// TestAvg (func_avg_test.go).
#[test]
fn avg() {
    for tp in [TypeNewDecimal, TypeDouble] {
        test_agg_func(&agg_tester(
            ast::AggFuncAvg,
            tp,
            5,
            vec![null(), new_float64_datum(2.0)],
        ));
    }
}

// TestMergePartialResult4FirstRow (func_first_row_test.go).
#[test]
fn merge_partial_result_first_row() {
    let collation = mysql::DefaultCollationName;
    let enum_c = parse_enum_name(&elems(), b"c", collation).unwrap();
    let enum_e = parse_enum_name(&elems(), b"e", collation).unwrap();
    let set_ed = parse_set_name(&elems(), b"e,d", collation).unwrap();
    let set_e = parse_set_name(&elems(), b"e", collation).unwrap();
    let enum_d =
        |e: &tidb_types::enum_type::Enum| new_collate_mysql_enum_datum(e.clone(), collation);
    let set_d = |s: &tidb_types::set_type::Set| new_mysql_set_datum(s.clone(), collation);
    let (i, f, s) = (new_int_datum, new_float64_datum, |v: &str| {
        new_string_datum(v)
    });
    let du = |n| new_duration_datum(dur(n));
    let name = ast::AggFuncFirstRow;
    for p in [
        agg_tester(name, TypeLonglong, 5, vec![i(0), i(2), i(0)]),
        agg_tester(name, TypeFloat, 5, vec![f(0.0), f(2.0), f(0.0)]),
        agg_tester(name, TypeDouble, 5, vec![f(0.0), f(2.0), f(0.0)]),
        agg_tester(name, TypeNewDecimal, 5, vec![dec(0), dec(2), dec(0)]),
        agg_tester(name, TypeString, 5, vec![s("0"), s("2"), s("0")]),
        agg_tester(name, TypeDate, 5, vec![date(365), date(367), date(365)]),
        agg_tester(name, TypeDuration, 5, vec![du(0), du(2), du(0)]),
        agg_tester(name, TypeJSON, 5, vec![json(0), json(2), json(0)]),
        agg_tester(
            name,
            TypeEnum,
            5,
            vec![enum_d(&enum_e), enum_d(&enum_c), enum_d(&enum_e)],
        ),
        agg_tester(
            name,
            TypeSet,
            5,
            vec![set_d(&set_e), set_d(&set_ed), set_d(&set_e)],
        ),
    ] {
        test_merge_partial_result(&p);
    }
}

// TestMergePartialResult4MaxMin (func_max_min_test.go).
#[test]
fn merge_partial_result_max_min() {
    let collation = mysql::DefaultCollationName;
    let enum_a = parse_enum(&elems(), b"a", collation).unwrap();
    let enum_c = parse_enum(&elems(), b"c", collation).unwrap();
    let enum_e = parse_enum(&elems(), b"e", collation).unwrap();
    let set_c = parse_set(&elems(), b"c", collation).unwrap(); // set_c.value == 4
    let set_ed = parse_set(&elems(), b"e,d", collation).unwrap(); // set_ed.value == 3
    let enum_d =
        |e: &tidb_types::enum_type::Enum| new_collate_mysql_enum_datum(e.clone(), collation);
    let set_d = |s: &tidb_types::set_type::Set| new_mysql_set_datum(s.clone(), collation);
    let (i, f, s) = (new_int_datum, new_float64_datum, |v: &str| {
        new_string_datum(v)
    });
    let du = |n| new_duration_datum(dur(n));
    let (max, min) = (ast::AggFuncMax, ast::AggFuncMin);
    for p in [
        agg_tester(max, TypeLonglong, 5, vec![i(4), i(4), i(4)]),
        agg_tester_with_field_type(max, unsigned_type(), 5, vec![i(4), i(4), i(4)]),
        agg_tester(max, TypeFloat, 5, vec![f(4.0), f(4.0), f(4.0)]),
        agg_tester(max, TypeDouble, 5, vec![f(4.0), f(4.0), f(4.0)]),
        agg_tester(max, TypeNewDecimal, 5, vec![dec(4), dec(4), dec(4)]),
        agg_tester(max, TypeString, 5, vec![s("4"), s("4"), s("4")]),
        agg_tester(max, TypeDate, 5, vec![date(369), date(369), date(369)]),
        agg_tester(max, TypeDuration, 5, vec![du(4), du(4), du(4)]),
        agg_tester(max, TypeJSON, 5, vec![json(4), json(4), json(4)]),
        agg_tester(
            max,
            TypeEnum,
            5,
            vec![enum_d(&enum_e), enum_d(&enum_c), enum_d(&enum_e)],
        ),
        agg_tester(
            max,
            TypeSet,
            5,
            vec![set_d(&set_ed), set_d(&set_ed), set_d(&set_ed)],
        ),
        agg_tester(min, TypeLonglong, 5, vec![i(0), i(2), i(0)]),
        agg_tester_with_field_type(min, unsigned_type(), 5, vec![i(0), i(2), i(0)]),
        agg_tester(min, TypeFloat, 5, vec![f(0.0), f(2.0), f(0.0)]),
        agg_tester(min, TypeDouble, 5, vec![f(0.0), f(2.0), f(0.0)]),
        agg_tester(min, TypeNewDecimal, 5, vec![dec(0), dec(2), dec(0)]),
        agg_tester(min, TypeString, 5, vec![s("0"), s("2"), s("0")]),
        agg_tester(min, TypeDate, 5, vec![date(365), date(367), date(365)]),
        agg_tester(min, TypeDuration, 5, vec![du(0), du(2), du(0)]),
        agg_tester(min, TypeJSON, 5, vec![json(0), json(2), json(0)]),
        agg_tester(
            min,
            TypeEnum,
            5,
            vec![enum_d(&enum_a), enum_d(&enum_a), enum_d(&enum_a)],
        ),
        agg_tester(
            min,
            TypeSet,
            5,
            vec![set_d(&set_c), set_d(&set_c), set_d(&set_c)],
        ),
    ] {
        test_merge_partial_result(&p);
    }
}

// TestMaxMin (func_max_min_test.go).
#[test]
fn max_min() {
    let (i, f, s) = (new_int_datum, new_float64_datum, |v: &str| {
        new_string_datum(v)
    });
    let du = |n| new_duration_datum(dur(n));
    let (max, min) = (ast::AggFuncMax, ast::AggFuncMin);
    for p in [
        agg_tester(max, TypeLonglong, 5, vec![null(), i(4)]),
        agg_tester_with_field_type(max, unsigned_type(), 5, vec![null(), i(4)]),
        agg_tester(max, TypeFloat, 5, vec![null(), f(4.0)]),
        agg_tester(max, TypeDouble, 5, vec![null(), f(4.0)]),
        agg_tester(max, TypeNewDecimal, 5, vec![null(), dec(4)]),
        agg_tester(max, TypeString, 5, vec![null(), s("4"), s("4")]),
        agg_tester(max, TypeDate, 5, vec![null(), date(369)]),
        agg_tester(max, TypeDuration, 5, vec![null(), du(4)]),
        agg_tester(max, TypeJSON, 5, vec![null(), json(4)]),
        agg_tester(min, TypeLonglong, 5, vec![null(), i(0)]),
        agg_tester_with_field_type(min, unsigned_type(), 5, vec![null(), i(0)]),
        agg_tester(min, TypeFloat, 5, vec![null(), f(0.0)]),
        agg_tester(min, TypeDouble, 5, vec![null(), f(0.0)]),
        agg_tester(min, TypeNewDecimal, 5, vec![null(), dec(0)]),
        agg_tester(min, TypeString, 5, vec![null(), s("0")]),
        agg_tester(min, TypeDate, 5, vec![null(), date(365)]),
        agg_tester(min, TypeDuration, 5, vec![null(), du(0)]),
        agg_tester(min, TypeJSON, 5, vec![null(), json(0)]),
    ] {
        test_agg_func(&p);
    }
}

// TestMergePartialResult4BitFuncs (func_bitfuncs_test.go).
#[test]
fn merge_partial_result_bit_funcs() {
    let i = new_int_datum;
    for p in [
        agg_tester(ast::AggFuncBitAnd, TypeLonglong, 5, vec![i(0), i(0), i(0)]),
        agg_tester(ast::AggFuncBitOr, TypeLonglong, 5, vec![i(7), i(7), i(7)]),
        agg_tester(ast::AggFuncBitXor, TypeLonglong, 5, vec![i(4), i(5), i(1)]),
    ] {
        test_merge_partial_result(&p);
    }
}

// TestMemSum / TestMemAvg (func_sum_test.go, func_avg_test.go), DISTINCT cases: Go counts
// the set's growth in memDelta; Rust allocates the set through the allocator handed over
// with `set_alloc`, so the tracker grows with it and is given back when the partial result
// is dropped (spec §5.3).
#[test]
fn distinct_sets_are_accounted() {
    for name in [ast::AggFuncSum, ast::AggFuncAvg] {
        for tp in [TypeDouble, TypeNewDecimal] {
            let ctx = es::new_expr_context(vec![]);
            let ec = ctx.get_eval_ctx();
            let ec: &dyn EvalContext = &*ec;
            let ft = new_field_type(tp);
            let desc = new_agg_func_desc(&*ctx, name, vec![column(&ft, 0)], true).unwrap();
            let t = new_tracker(1, -1);
            {
                let a = TrackedAlloc::new(Arc::clone(&t));
                let mut f = built(&ctx, &desc);
                f.set_alloc(ColAlloc::Tracked(a.clone()));
                let mut pr = f.alloc_partial_result();
                let p = agg_tester(name, tp, 1000, vec![]);
                let src = gen_src_chk(&p);
                update_each(ec, &*f, &rows(&src), &mut pr).unwrap();
                a.flush();
                assert!(
                    t.bytes_consumed() > 0,
                    "{name} DISTINCT {tp}: no growth accounted"
                );
                // A reset partial result starts an empty set in the same allocator.
                f.reset_partial_result(&mut pr);
                update_each(ec, &*f, &rows(&src), &mut pr).unwrap();
                a.flush();
                assert!(t.bytes_consumed() > 0);
            }
            assert_eq!(
                t.bytes_consumed(),
                0,
                "{name} DISTINCT {tp}: memory not given back"
            );
        }
    }
}

// TestMemCount / TestMemSum / TestMemAvg / TestMemFirstRow / TestMemMaxMin / TestMemBitFunc:
// Go checks that AllocPartialResult and ResetPartialResult report the same size; Rust checks
// that a reset partial result behaves exactly as a fresh one, in every mode of every function.
#[test]
fn reset_partial_result_is_fresh() {
    let names = [
        ast::AggFuncCount,
        ast::AggFuncSum,
        ast::AggFuncAvg,
        ast::AggFuncFirstRow,
        ast::AggFuncMax,
        ast::AggFuncMin,
        ast::AggFuncBitOr,
        ast::AggFuncBitXor,
        ast::AggFuncBitAnd,
    ];
    let mut types = ALL_TYPES.to_vec();
    types.extend([TypeEnum, TypeSet]);
    let mut checked = 0;
    for name in names {
        for &tp in &types {
            for distinct in [false, true] {
                if distinct && name == ast::AggFuncCount {
                    continue; // part B1
                }
                let p = agg_tester(name, tp, 5, vec![]);
                let ctx = es::new_expr_context(vec![]);
                let ec = ctx.get_eval_ctx();
                let ec: &dyn EvalContext = &*ec;
                let Ok(mut desc) =
                    new_agg_func_desc(&*ctx, name, vec![column(&p.data_type, 0)], distinct)
                else {
                    continue;
                };
                // As the planner does before building the executors.
                desc.base.wrap_cast_for_agg_args(&*ctx).unwrap();
                let (partial, final_) = desc.split(&[0, 1]);
                for d in [&desc, &partial, &final_] {
                    let Some(f) = build(&*ctx, d, 0).unwrap() else {
                        continue;
                    };
                    if d.mode == AggFunctionMode::FinalMode {
                        // The final phase reads the partial results, not the source rows.
                        continue;
                    }
                    let mut fresh = f.alloc_partial_result();
                    let want = final_datum(ec, &*f, &mut fresh, &d.ret_tp);
                    let mut pr = f.alloc_partial_result();
                    let src = gen_src_chk(&p);
                    update_each(ec, &*f, &rows(&src), &mut pr).unwrap();
                    f.reset_partial_result(&mut pr);
                    let got = final_datum(ec, &*f, &mut pr, &d.ret_tp);
                    let what = format!("{name} distinct={distinct} type {tp} ({})", f.type_name());
                    assert_eq!(got.is_null(), want.is_null(), "{what}");
                    assert_datum_eq(ec, &got, &want, p.data_type.get_collate());
                    checked += 1;
                }
            }
        }
    }
    assert!(checked > 300, "{checked} functions checked");
}

// A final function merges a partial result into its own freshly allocated one.
#[test]
fn merge_into_fresh_final() {
    let ctx = es::new_expr_context(vec![]);
    let ec = ctx.get_eval_ctx();
    let ec: &dyn EvalContext = &*ec;
    let p = agg_tester(ast::AggFuncSum, TypeNewDecimal, 5, vec![]);
    let desc = new_agg_func_desc(&*ctx, p.func_name, vec![column(&p.data_type, 0)], false).unwrap();
    let (partial_desc, final_desc) = desc.split(&[0, 1]);
    let partial = built(&ctx, &partial_desc);
    let fin = built(&ctx, &final_desc);
    let mut src_pr = partial.alloc_partial_result();
    let src = gen_src_chk(&p);
    update_each(ec, &*partial, &rows(&src), &mut src_pr).unwrap();
    let mut dst = fin.alloc_partial_result();
    fin.merge_partial_result(ec, &mut src_pr, &mut dst).unwrap();
    let dt = final_datum(ec, &*fin, &mut dst, &final_desc.ret_tp);
    assert_datum_eq(ec, &dt, &dec(10), p.data_type.get_collate());
}

// Spec §5.1: one function object is shared by the partial workers, and partial results move
// between them.
#[test]
fn shared_between_workers() {
    fn assert_send_sync<T: Send + Sync + ?Sized>() {}
    fn assert_send<T: Send + ?Sized>() {}
    assert_send_sync::<dyn AggFunc>();
    assert_send::<PartialResult>();
}

// NewAggPartialResultMapper / NewAggPartialResultMapperWithCap (aggfuncs.go), which no
// govec record reaches: the empty map of partial results of the hash aggregation.
#[test]
fn partial_result_mapper() {
    let mut m = new_agg_partial_result_mapper();
    assert!(m.empty());
    assert_eq!(m.len(), 0);
    let ctx = es::new_expr_context(vec![]);
    let ft = new_field_type(TypeLonglong);
    let desc = new_agg_func_desc(&*ctx, ast::AggFuncCount, vec![column(&ft, 0)], false).unwrap();
    let f = built(&ctx, &desc);
    m.set(b"group".to_vec(), vec![f.alloc_partial_result()]);
    assert!(m.exist(&b"group".to_vec()));
    assert_eq!(m.len(), 1);
    assert!(new_agg_partial_result_mapper_with_cap(0).empty());
}

// baseAggFunc.MergePartialResult (aggfuncs.go), which no govec record reaches: the functions
// that read the original rows merge nothing (only the final functions of a split merge).
#[test]
fn original_functions_merge_nothing() {
    let ctx = es::new_expr_context(vec![]);
    let ec = ctx.get_eval_ctx();
    let ec: &dyn EvalContext = &*ec;
    let p = agg_tester(ast::AggFuncCount, TypeLonglong, 5, vec![]);
    let desc = new_agg_func_desc(&*ctx, p.func_name, vec![column(&p.data_type, 0)], false).unwrap();
    let f = built(&ctx, &desc);
    assert!(f.type_name().ends_with("::CountOriginal4Int"));
    let mut src = f.alloc_partial_result();
    let chk = gen_src_chk(&p);
    update_each(ec, &*f, &rows(&chk), &mut src).unwrap();
    let mut dst = f.alloc_partial_result();
    f.merge_partial_result(ec, &mut src, &mut dst).unwrap();
    let ret = Arc::clone(&desc.ret_tp);
    assert_datum_eq(
        ec,
        &final_datum(ec, &*f, &mut dst, &ret),
        &new_int_datum(0),
        "binary",
    );
    assert_datum_eq(
        ec,
        &final_datum(ec, &*f, &mut src, &ret),
        &new_int_datum(5),
        "binary",
    );
}
