//! The unit tests of Go's `aggfuncs` for the functions of parts A, B1, and B2 (spec D12.4 §6):
//! `TestCount`, `TestSum`, `TestAvg`, `TestMaxMin`, `TestGroupConcat`, `TestJsonArrayagg`,
//! `TestJsonObjectagg`, `TestVarpop`, `TestVarsamp`, `TestStddevpop`, `TestStddevsamp`,
//! `TestWriteTime`, `TestPercentile`, `TestFix26807`, `TestFix40463`, and
//! `TestMergePartialResult4*`, with Go's helpers `testAggFunc`, `testMultiArgsAggFunc`, and
//! `testMergePartialResult` (`aggfunc_test.go`), plus the `TestMem*` tests as properties of
//! the Rust memory accounting.
//!
//! Differences from the Go tests:
//! - Go's `messUpChunk` overwrites the source chunk after each update, to catch partial
//!   results that keep a reference into it. A Rust partial result cannot borrow the chunk
//!   (`PartialResult` is `'static`), so the helpers drop it instead.
//! - `TestMem*` check Go's estimated `memDelta`, which Rust does not return (the memory
//!   exception A1): the growing partial result containers grow a memory tracker through the
//!   allocator and give it back when dropped, and every function's reset partial result
//!   behaves as a fresh one.

// Go identifiers for the type constants in patterns (M-GONAME).
#![allow(non_upper_case_globals)]

mod window;

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::sync::Arc;

use tidb_base::Error;
use tidb_base::contextutil::WarnHandler;
use tidb_base::dbterror::plannererrors;
use tidb_base::farm;
use tidb_base::mysql::{
    self, TypeDate, TypeDouble, TypeDuration, TypeEnum, TypeFloat, TypeJSON, TypeLonglong,
    TypeNewDecimal, TypeSet, TypeString, UnsignedFlag,
};
use tidb_base::selection;
use tidb_chunk::ColAlloc;
use tidb_chunk::chunk::{Chunk, new_chunk_with_capacity};
use tidb_chunk::row::Row;
use tidb_expression::aggregation::{AggFuncDesc, AggFunctionMode, new_agg_func_desc};
use tidb_expression::ast_funcs as ast;
use tidb_expression::byitem::ByItems;
use tidb_expression::context::{BuildContext, EvalContext, type_ctx};
use tidb_expression::errors::ErrCutValueGroupConcat;
use tidb_expression::expression::EvalState;
use tidb_expression::{Column, Constant, Expr, ExprRef};
use tidb_kv::set::new_string_set_with_memory_usage;
use tidb_memory::TrackedAlloc;
use tidb_memory::tracker::new_tracker;
use tidb_parser::gocharset::CharsetBin;
use tidb_parser::types::FieldType;
use tidb_sessionctx::exprstatic as es;
use tidb_types::collate::get_collator;
use tidb_types::context::{FlagTruncateAsWarning, Flags};
use tidb_types::datum::{
    Datum, new_bytes_datum, new_collate_mysql_enum_datum, new_decimal_datum, new_duration_datum,
    new_float32_datum, new_float64_datum, new_int_datum, new_json_datum, new_mysql_set_datum,
    new_string_datum, new_time_datum,
};
use tidb_types::enum_type::{parse_enum, parse_enum_name, parse_enum_value};
use tidb_types::field_type::new_field_type;
use tidb_types::json::{
    ErrInvalidJSONCharset, ErrJSONDocumentNULLKey, ErrUnsupportedSecondArgumentType, JSONValue,
    Opaque, create_binary_json,
};
use tidb_types::mydecimal::new_dec_from_int;
use tidb_types::set_type::{parse_set, parse_set_name, parse_set_value};
use tidb_types::time::{Duration, parse_date, time_from_days};

use super::func_count_distinct::{
    eval_and_encode, eval_and_encode_go_raw, new_partial_result_4_approx_count_distinct, write_time,
};
use super::func_percentile::percentile;
use super::{
    AggFunc, PartialResult, build, new_agg_partial_result_mapper,
    new_agg_partial_result_mapper_with_cap,
};

const SEPARATOR: &str = " ";

// aggTest (aggfunc_test.go).
struct AggTest {
    data_type: FieldType,
    num_rows: usize,
    func_name: &'static str,
    results: Vec<Datum>,
    order_by: bool,
}

// multiArgsAggTest (aggfunc_test.go).
struct MultiArgsAggTest {
    data_types: Vec<FieldType>,
    num_rows: usize,
    func_name: &'static str,
    results: Vec<Datum>,
    order_by: bool,
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
        order_by: false,
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
        order_by: false,
    }
}

fn sep_const(sep: &str) -> ExprRef {
    Expr::Constant(Constant::new(
        new_string_datum(sep),
        new_field_type(TypeString),
    ))
    .into_ref()
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
        .expect("the aggregate function builds")
        .expect("the descriptor has a function")
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

// testAggFuncWithoutDistinct (aggfunc_test.go).
fn test_agg_func_without_distinct(p: &AggTest) {
    let ctx = es::new_expr_context(vec![]);
    let ec = ctx.get_eval_ctx();
    let ec: &dyn EvalContext = &*ec;
    let mut args = vec![column(&p.data_type, 0)];
    let collate = p.data_type.get_collate();
    if p.func_name == ast::AggFuncGroupConcat {
        args.push(sep_const(SEPARATOR));
    }
    if p.func_name == ast::AggFuncApproxPercentile {
        args.push(
            Expr::Constant(Constant::new(
                new_int_datum(50),
                new_field_type(mysql::TypeLong),
            ))
            .into_ref(),
        );
    }

    let mut desc = new_agg_func_desc(&*ctx, p.func_name, args.clone(), false).unwrap();
    if p.order_by {
        desc.order_by_items = vec![ByItems {
            expr: Arc::clone(&args[0]),
            desc: true,
        }];
    }
    let mut final_func = built(&ctx, &desc);
    if p.func_name == ast::AggFuncApproxPercentile {
        final_func.set_rng_seed_for_test(42);
    }
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
}

// testAggFunc (aggfunc_test.go).
fn test_agg_func(p: &AggTest) {
    test_agg_func_without_distinct(p);

    let ctx = es::new_expr_context(vec![]);
    let ec = ctx.get_eval_ctx();
    let ec: &dyn EvalContext = &*ec;
    let mut args = vec![column(&p.data_type, 0)];
    let collate = p.data_type.get_collate();
    if p.func_name == ast::AggFuncGroupConcat {
        args.push(sep_const(SEPARATOR));
    }
    if p.func_name == ast::AggFuncApproxPercentile {
        args.push(
            Expr::Constant(Constant::new(
                new_int_datum(50),
                new_field_type(mysql::TypeLong),
            ))
            .into_ref(),
        );
    }

    // test the agg func with distinct
    let mut desc = new_agg_func_desc(&*ctx, p.func_name, args.clone(), true).unwrap();
    if p.order_by {
        desc.order_by_items = vec![ByItems {
            expr: Arc::clone(&args[0]),
            desc: true,
        }];
    }
    let mut final_func = built(&ctx, &desc);
    if p.func_name == ast::AggFuncApproxPercentile {
        final_func.set_rng_seed_for_test(42);
    }
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
    test_multi_args_agg_func_with_ctx(&ctx, p);
}

fn test_multi_args_agg_func_with_ctx(ctx: &es::ExprContext, p: &MultiArgsAggTest) {
    let ec = ctx.get_eval_ctx();
    let ec: &dyn EvalContext = &*ec;
    let mut args: Vec<ExprRef> = p
        .data_types
        .iter()
        .enumerate()
        .map(|(k, ft)| column(ft, k as isize))
        .collect();
    if p.func_name == ast::AggFuncGroupConcat {
        args.push(sep_const(SEPARATOR));
    }

    let mut desc = new_agg_func_desc(ctx, p.func_name, args.clone(), false).unwrap();
    if p.order_by {
        desc.order_by_items = vec![ByItems {
            expr: Arc::clone(&args[0]),
            desc: true,
        }];
    }
    let collate = args[0].get_type(ec).get_collate();
    let final_func = built(ctx, &desc);
    let mut final_pr = final_func.alloc_partial_result();
    let src = gen_multi_src_chk(p);
    let st = EvalState::new();
    for row in rows(&src) {
        // As Go's FIXME: JSON_OBJECTAGG rejects the trailing NULL key row from genSrcChk.
        let _ = final_func.update_partial_result(ec, &st, &[row], &mut final_pr);
    }
    let dt = final_datum(ec, &*final_func, &mut final_pr, &desc.ret_tp);
    assert_datum_eq(ec, &dt, &p.results[1], collate);

    // test the empty input
    final_func.reset_partial_result(&mut final_pr);
    let dt = final_datum(ec, &*final_func, &mut final_pr, &desc.ret_tp);
    assert_datum_eq(ec, &dt, &p.results[0], collate);

    // test the agg func with distinct
    let mut desc = new_agg_func_desc(ctx, p.func_name, args.clone(), true).unwrap();
    if p.order_by {
        desc.order_by_items = vec![ByItems {
            expr: Arc::clone(&args[0]),
            desc: true,
        }];
    }
    let final_func = built(ctx, &desc);
    let mut final_pr = final_func.alloc_partial_result();
    for _ in 0..2 {
        let src = gen_multi_src_chk(p);
        for row in rows(&src) {
            let _ = final_func.update_partial_result(ec, &st, &[row], &mut final_pr);
        }
    }
    let dt = final_datum(ec, &*final_func, &mut final_pr, &desc.ret_tp);
    assert_datum_eq(ec, &dt, &p.results[1], collate);
}

// testMergePartialResult (aggfunc_test.go).
fn test_merge_partial_result(p: &AggTest) {
    let ctx = es::new_expr_context(vec![]);
    let ec = ctx.get_eval_ctx();
    let ec: &dyn EvalContext = &*ec;
    let mut args = vec![column(&p.data_type, 0)];
    let collate = p.data_type.get_collate();
    if p.func_name == ast::AggFuncGroupConcat {
        args.push(sep_const(SEPARATOR));
    }
    let mut desc = new_agg_func_desc(&*ctx, p.func_name, args.clone(), false).unwrap();
    if p.order_by {
        desc.order_by_items = vec![ByItems {
            expr: Arc::clone(&args[0]),
            desc: true,
        }];
    }
    let (partial_desc, final_desc) = desc.split(&[0, 1]);

    // build partial func for partial phase.
    let partial_func = built(&ctx, &partial_desc);
    let mut partial_result = partial_func.alloc_partial_result();

    // build final func for final phase.
    let final_func = built(&ctx, &final_desc);
    let mut final_pr = final_func.alloc_partial_result();
    let partial_out_ft = if p.func_name == ast::AggFuncApproxCountDistinct {
        new_field_type(TypeString)
    } else if p.func_name == ast::AggFuncJsonArrayagg {
        new_field_type(TypeJSON)
    } else {
        p.data_type.clone()
    };
    let final_out_ft = if p.func_name == ast::AggFuncApproxCountDistinct {
        new_field_type(TypeLonglong)
    } else if p.func_name == ast::AggFuncJsonArrayagg {
        new_field_type(TypeJSON)
    } else {
        p.data_type.clone()
    };

    // update partial result.
    let src = gen_src_chk(p);
    update_each(ec, &*partial_func, &rows(&src), &mut partial_result).unwrap();
    drop(src);
    let dt = final_datum(ec, &*partial_func, &mut partial_result, &partial_out_ft);
    assert_datum_eq(ec, &dt, &p.results[0], collate);

    final_func
        .merge_partial_result(ec, &mut partial_result, &mut final_pr)
        .unwrap();
    partial_func.reset_partial_result(&mut partial_result);

    let src = gen_src_chk(p);
    update_each(ec, &*partial_func, &rows(&src)[2..], &mut partial_result).unwrap();
    drop(src);
    let dt = final_datum(ec, &*partial_func, &mut partial_result, &partial_out_ft);
    assert_datum_eq(ec, &dt, &p.results[1], collate);
    final_func
        .merge_partial_result(ec, &mut partial_result, &mut final_pr)
        .unwrap();

    let dt = final_datum(ec, &*final_func, &mut final_pr, &final_out_ft);
    assert_datum_eq(ec, &dt, &p.results[2], collate);
}

// testMultiArgsMergePartialResult (aggfunc_test.go).
fn test_multi_args_merge_partial_result(p: &MultiArgsAggTest, ret_tp: &FieldType) {
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
    let (partial_desc, final_desc) = desc.split(&[0, 1]);

    let partial_func = built(&ctx, &partial_desc);
    let mut partial_result = partial_func.alloc_partial_result();

    let final_func = built(&ctx, &final_desc);
    let mut final_pr = final_func.alloc_partial_result();
    let st = EvalState::new();

    let src = gen_multi_src_chk(p);
    for row in rows(&src) {
        let _ = partial_func.update_partial_result(ec, &st, &[row], &mut partial_result);
    }
    drop(src);
    let dt = final_datum(ec, &*partial_func, &mut partial_result, ret_tp);
    assert_datum_eq(ec, &dt, &p.results[0], collate);

    final_func
        .merge_partial_result(ec, &mut partial_result, &mut final_pr)
        .unwrap();
    partial_func.reset_partial_result(&mut partial_result);

    let src = gen_multi_src_chk(p);
    for row in &rows(&src)[2..] {
        let _ = partial_func.update_partial_result(ec, &st, &[*row], &mut partial_result);
    }
    drop(src);
    let dt = final_datum(ec, &*partial_func, &mut partial_result, ret_tp);
    assert_datum_eq(ec, &dt, &p.results[1], collate);

    final_func
        .merge_partial_result(ec, &mut partial_result, &mut final_pr)
        .unwrap();
    let dt = final_datum(ec, &*final_func, &mut final_pr, ret_tp);
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

// genApproxDistinctMergePartialResult (func_count_test.go).
fn gen_approx_distinct_merge_partial_result(begin: u64, end: u64) -> Vec<u8> {
    let mut o = new_partial_result_4_approx_count_distinct();
    for i in begin..end {
        let x = farm::hash64(&i.to_le_bytes());
        o.insert_hash64(x);
    }
    o.serialize()
}

// TestMergePartialResult4Count (func_count_test.go), including APPROX_COUNT_DISTINCT.
#[test]
fn merge_partial_result_count() {
    let i = new_int_datum;
    test_merge_partial_result(&agg_tester(
        ast::AggFuncCount,
        TypeLonglong,
        5,
        vec![i(5), i(3), i(8)],
    ));
    test_merge_partial_result(&agg_tester(
        ast::AggFuncApproxCountDistinct,
        TypeLonglong,
        5,
        vec![
            new_bytes_datum(gen_approx_distinct_merge_partial_result(0, 5)),
            new_bytes_datum(gen_approx_distinct_merge_partial_result(2, 5)),
            i(5),
        ],
    ));
}

// TestCount (func_count_test.go), including APPROX_COUNT_DISTINCT.
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
    for tp in ALL_TYPES {
        let results = vec![new_int_datum(0), new_int_datum(5)];
        test_multi_args_agg_func(&multi_args_agg_tester(
            ast::AggFuncApproxCountDistinct,
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

    // The keys of a string set are in the set's allocator too (Go adds `len(decStr)` to
    // memDelta, func_sum.go:400, func_avg.go:268): 1000 keys of 1 KiB account at least 1 MiB.
    let t = new_tracker(1, -1);
    {
        let a = TrackedAlloc::new(Arc::clone(&t));
        let (mut s, _) = new_string_set_with_memory_usage(a.clone());
        for i in 0..1000u32 {
            let mut key = vec![0u8; 1024];
            key[..4].copy_from_slice(&i.to_le_bytes());
            s.insert(&key);
        }
        assert_eq!(s.count(), 1000);
        a.flush();
        assert!(
            t.bytes_consumed() >= 1 << 20,
            "string set keys not accounted: {} bytes",
            t.bytes_consumed()
        );
    }
    assert_eq!(t.bytes_consumed(), 0, "string set keys not given back");
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
    let mut key = allocator_api2::vec::Vec::new();
    key.extend_from_slice(b"group");
    let key = key.into_boxed_slice();
    let mut prs = allocator_api2::vec::Vec::new();
    prs.push(f.alloc_partial_result());
    m.set(key, prs);
    assert!(m.m.contains_key(b"group".as_slice()));
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

// TestMergePartialResult4Varpop (func_varpop_test.go).
#[test]
fn merge_partial_result_varpop() {
    let f = new_float64_datum;
    test_merge_partial_result(&agg_tester(
        ast::AggFuncVarPop,
        TypeDouble,
        5,
        vec![
            f(2.0),
            f(2.0 / 3.0),
            f(59.0 / 8.0 - (19.0 * 19.0) / (8.0 * 8.0)),
        ],
    ));
}

// TestVarpop (func_varpop_test.go).
#[test]
fn varpop() {
    test_agg_func(&agg_tester(
        ast::AggFuncVarPop,
        TypeDouble,
        5,
        vec![null(), new_float64_datum(2.0)],
    ));
}

// TestMergePartialResult4Varsamp (func_varsamp_test.go).
#[test]
fn merge_partial_result_varsamp() {
    let f = new_float64_datum;
    test_merge_partial_result(&agg_tester(
        ast::AggFuncVarSamp,
        TypeDouble,
        5,
        vec![f(2.5), f(1.0), f(1.9821428571428572)],
    ));
}

// TestVarsamp (func_varsamp_test.go).
#[test]
fn varsamp() {
    test_agg_func(&agg_tester(
        ast::AggFuncVarSamp,
        TypeDouble,
        5,
        vec![null(), new_float64_datum(2.5)],
    ));
}

// TestMergePartialResult4Stddevpop (func_stddevpop_test.go).
#[test]
fn merge_partial_result_stddevpop() {
    let f = new_float64_datum;
    test_merge_partial_result(&agg_tester(
        ast::AggFuncStddevPop,
        TypeDouble,
        5,
        vec![
            f(std::f64::consts::SQRT_2),
            f(0.816496580927726),
            f(1.3169567191065923),
        ],
    ));
}

// TestStddevpop (func_stddevpop_test.go).
#[test]
fn stddevpop() {
    test_agg_func(&agg_tester(
        ast::AggFuncStddevPop,
        TypeDouble,
        5,
        vec![null(), new_float64_datum(std::f64::consts::SQRT_2)],
    ));
}

// TestMergePartialResult4Stddevsamp (func_stddevsamp_test.go).
#[test]
fn merge_partial_result_stddevsamp() {
    let f = new_float64_datum;
    test_merge_partial_result(&agg_tester(
        ast::AggFuncStddevSamp,
        TypeDouble,
        5,
        vec![f(1.5811388300841898), f(1.0), f(1.407885953173359)],
    ));
}

// TestStddevsamp (func_stddevsamp_test.go).
#[test]
fn stddevsamp() {
    test_agg_func(&agg_tester(
        ast::AggFuncStddevSamp,
        TypeDouble,
        5,
        vec![null(), new_float64_datum(1.5811388300841898)],
    ));
}

// Single-row VAR_SAMP / STDDEV_SAMP returns NULL (count <= 1), while VAR_POP / STDDEV_POP returns 0.
#[test]
fn var_stddev_single_row_and_distinct() {
    let ctx = es::new_expr_context(vec![]);
    let ec = ctx.get_eval_ctx();
    let ec: &dyn EvalContext = &*ec;
    let ft = new_field_type(TypeDouble);
    for distinct in [false, true] {
        for (name, expect_null) in [
            (ast::AggFuncVarPop, false),
            (ast::AggFuncStddevPop, false),
            (ast::AggFuncVarSamp, true),
            (ast::AggFuncStddevSamp, true),
        ] {
            let desc = new_agg_func_desc(&*ctx, name, vec![column(&ft, 0)], distinct).unwrap();
            let f = built(&ctx, &desc);
            let mut pr = f.alloc_partial_result();
            let mut chk = new_chunk_with_capacity(std::slice::from_ref(&ft), 2);
            chk.append_float64(0, 42.0);
            if distinct {
                chk.append_float64(0, 42.0);
            }
            update_each(ec, &*f, &rows(&chk), &mut pr).unwrap();
            let dt = final_datum(ec, &*f, &mut pr, &desc.ret_tp);
            if expect_null {
                assert!(dt.is_null(), "{name} distinct={distinct}: expected NULL");
            } else {
                assert_datum_eq(ec, &dt, &new_float64_datum(0.0), ft.get_collate());
            }
        }
    }
}

// getJSONValue (func_json_objectagg_test.go).
fn test_json_value(d: &Datum, ft: &FieldType) -> JSONValue<'static> {
    if ft.get_type() == TypeString && ft.get_charset() == CharsetBin {
        let mut buf = vec![0u8; ft.get_flen() as usize];
        let src = d.get_bytes();
        let n = src.len().min(buf.len());
        buf[..n].copy_from_slice(&src[..n]);
        return JSONValue::Opaque(Opaque {
            type_code: TypeString,
            buf: Cow::Owned(buf),
        });
    }
    match ft.get_type() {
        TypeLonglong => JSONValue::Int64(d.get_int64()),
        TypeFloat => JSONValue::Float64(f64::from(d.get_float32())),
        TypeDouble => JSONValue::Float64(d.get_float64()),
        TypeString => JSONValue::String(d.get_bytes().to_vec()),
        TypeJSON => JSONValue::BinaryJSON(d.get_mysql_json().into_owned()),
        TypeDate => JSONValue::Time(d.get_mysql_time()),
        TypeDuration => JSONValue::Duration(d.get_mysql_duration()),
        tp => panic!("unexpected json test type {tp}"),
    }
}

// TestMergePartialResult4JsonArrayagg (func_json_arrayagg_test.go).
#[test]
fn merge_partial_result_json_arrayagg() {
    let type_list = [
        TypeLonglong,
        TypeDouble,
        TypeFloat,
        TypeString,
        TypeJSON,
        TypeDate,
        TypeDuration,
    ];
    let num_rows = 5;
    for arg_type in type_list {
        let arg_ft = new_field_type(arg_type);
        let mut entries1 = Vec::new();
        for m in 0..num_rows {
            entries1.push(test_json_value(&data_gen(&arg_ft, m), &arg_ft));
        }
        entries1.push(JSONValue::Nil);

        let mut entries2 = Vec::new();
        for m in 2..num_rows {
            entries2.push(test_json_value(&data_gen(&arg_ft, m), &arg_ft));
        }
        entries2.push(JSONValue::Nil);

        let mut entries3 = entries1.clone();
        entries3.extend(entries2.iter().cloned());

        test_merge_partial_result(&agg_tester(
            ast::AggFuncJsonArrayagg,
            arg_type,
            num_rows,
            vec![
                new_json_datum(create_binary_json(JSONValue::Array(entries1))),
                new_json_datum(create_binary_json(JSONValue::Array(entries2))),
                new_json_datum(create_binary_json(JSONValue::Array(entries3))),
            ],
        ));
    }
}

// TestJsonArrayagg (func_json_arrayagg_test.go).
#[test]
fn json_arrayagg() {
    let type_list = [
        TypeLonglong,
        TypeDouble,
        TypeFloat,
        TypeString,
        TypeJSON,
        TypeDate,
        TypeDuration,
    ];
    let num_rows = 5;
    for arg_type in type_list {
        let arg_ft = new_field_type(arg_type);
        let mut entries = Vec::new();
        for m in 0..num_rows {
            entries.push(test_json_value(&data_gen(&arg_ft, m), &arg_ft));
        }
        entries.push(JSONValue::Nil);

        test_agg_func_without_distinct(&agg_tester(
            ast::AggFuncJsonArrayagg,
            arg_type,
            num_rows,
            vec![
                null(),
                new_json_datum(create_binary_json(JSONValue::Array(entries))),
            ],
        ));
    }
}

fn json_objectagg_types() -> Vec<FieldType> {
    let mut bin_str = new_field_type(TypeString);
    bin_str.set_flen(10);
    bin_str.set_charset(CharsetBin);
    vec![
        new_field_type(TypeLonglong),
        new_field_type(TypeDouble),
        new_field_type(TypeFloat),
        new_field_type(TypeString),
        new_field_type(TypeJSON),
        bin_str,
        new_field_type(TypeDate),
        new_field_type(TypeDuration),
    ]
}

// TestMergePartialResult4JsonObjectagg (func_json_objectagg_test.go).
#[test]
fn merge_partial_result_json_objectagg() {
    let type_list = json_objectagg_types();
    let num_rows = 5;
    let json_ft = new_field_type(TypeJSON);
    for key_ft in &type_list {
        if key_ft.get_charset() == CharsetBin {
            continue;
        }
        for val_ft in &type_list {
            let mut entries1 = BTreeMap::new();
            let mut entries2 = BTreeMap::new();
            for m in 0..num_rows {
                let first = data_gen(key_ft, m);
                let second = data_gen(val_ft, m);
                let key = first.to_string().unwrap();
                entries1.insert(key, test_json_value(&second, val_ft));
            }
            for m in 2..num_rows {
                let first = data_gen(key_ft, m);
                let second = data_gen(val_ft, m);
                let key = first.to_string().unwrap();
                entries2.insert(key, test_json_value(&second, val_ft));
            }
            let p = MultiArgsAggTest {
                data_types: vec![key_ft.clone(), val_ft.clone()],
                num_rows,
                func_name: ast::AggFuncJsonObjectAgg,
                results: vec![
                    new_json_datum(create_binary_json(JSONValue::Object(entries1.clone()))),
                    new_json_datum(create_binary_json(JSONValue::Object(entries2))),
                    new_json_datum(create_binary_json(JSONValue::Object(entries1))),
                ],
                order_by: false,
            };
            test_multi_args_merge_partial_result(&p, &json_ft);
        }
    }
}

// TestJsonObjectagg (func_json_objectagg_test.go).
#[test]
fn json_objectagg() {
    let type_list = json_objectagg_types();
    let num_rows = 5;
    for key_ft in &type_list {
        if key_ft.get_charset() == CharsetBin {
            continue;
        }
        for val_ft in &type_list {
            let mut entries = BTreeMap::new();
            for m in 0..num_rows {
                let first = data_gen(key_ft, m);
                let second = data_gen(val_ft, m);
                let key = first.to_string().unwrap();
                entries.insert(key, test_json_value(&second, val_ft));
            }
            let p = MultiArgsAggTest {
                data_types: vec![key_ft.clone(), val_ft.clone()],
                num_rows,
                func_name: ast::AggFuncJsonObjectAgg,
                results: vec![
                    null(),
                    new_json_datum(create_binary_json(JSONValue::Object(entries))),
                ],
                order_by: false,
            };
            test_multi_args_agg_func(&p);
        }
    }
}

// Error paths of JSON_OBJECTAGG (NULL key, binary charset key, unsupported value type) and
// JSON_ARRAYAGG (unsupported value type).
#[test]
fn json_agg_error_paths() {
    let ctx = es::new_expr_context(vec![]);
    let ec = ctx.get_eval_ctx();
    let ec: &dyn EvalContext = &*ec;
    let st = EvalState::new();

    // 1. NULL key -> ErrJSONDocumentNULLKey
    let str_ft = new_field_type(TypeString);
    let int_ft = new_field_type(TypeLonglong);
    let desc = new_agg_func_desc(
        &*ctx,
        ast::AggFuncJsonObjectAgg,
        vec![column(&str_ft, 0), column(&int_ft, 1)],
        false,
    )
    .unwrap();
    let f = built(&ctx, &desc);
    let mut pr = f.alloc_partial_result();
    let mut chk = new_chunk_with_capacity(&[str_ft.clone(), int_ft.clone()], 1);
    chk.append_null(0);
    chk.append_int64(1, 1);
    let err = f
        .update_partial_result(ec, &st, &rows(&chk), &mut pr)
        .unwrap_err();
    assert!(ErrJSONDocumentNULLKey.equal(&err), "{err:?}");

    // 2. Binary charset key -> ErrInvalidJSONCharset
    let mut bin_ft = new_field_type(TypeString);
    bin_ft.set_charset(CharsetBin);
    let desc = new_agg_func_desc(
        &*ctx,
        ast::AggFuncJsonObjectAgg,
        vec![column(&bin_ft, 0), column(&int_ft, 1)],
        false,
    )
    .unwrap();
    let f = built(&ctx, &desc);
    let mut pr = f.alloc_partial_result();
    let mut chk = new_chunk_with_capacity(&[bin_ft, int_ft], 1);
    chk.append_bytes(0, b"k");
    chk.append_int64(1, 1);
    let err = f
        .update_partial_result(ec, &st, &rows(&chk), &mut pr)
        .unwrap_err();
    assert!(ErrInvalidJSONCharset.equal(&err), "{err:?}");

    // 3. Unsupported value type (Enum) -> ErrUnsupportedSecondArgumentType
    let enum_ft = new_field_type(TypeEnum);
    for name in [ast::AggFuncJsonArrayagg, ast::AggFuncJsonObjectAgg] {
        let args = if name == ast::AggFuncJsonArrayagg {
            vec![column(&enum_ft, 0)]
        } else {
            vec![column(&str_ft, 0), column(&enum_ft, 1)]
        };
        let desc = new_agg_func_desc(&*ctx, name, args, false).unwrap();
        let f = built(&ctx, &desc);
        let mut pr = f.alloc_partial_result();
        let chk = if name == ast::AggFuncJsonArrayagg {
            let mut c = new_chunk_with_capacity(std::slice::from_ref(&enum_ft), 1);
            c.append_datum(0, &data_gen(&enum_ft, 0));
            c
        } else {
            let mut c = new_chunk_with_capacity(&[str_ft.clone(), enum_ft.clone()], 1);
            c.append_string(0, b"k");
            c.append_datum(1, &data_gen(&enum_ft, 0));
            c
        };
        let err = f
            .update_partial_result(ec, &st, &rows(&chk), &mut pr)
            .unwrap_err();
        assert!(
            ErrUnsupportedSecondArgumentType.equal(&err),
            "{name}: {err:?}"
        );
    }
}

// TestMemVarpop / TestMemJsonArrayagg / TestMemJsonObjectagg (A1 property tests, spec §5.3):
// growing partial results account through ColAlloc / TrackedAlloc and release on drop.
#[test]
fn var_stddev_and_json_agg_memory_accounted() {
    let ctx = es::new_expr_context(vec![]);
    let ec = ctx.get_eval_ctx();
    let ec: &dyn EvalContext = &*ec;

    // 1. VAR_POP / VAR_SAMP / STDDEV_POP / STDDEV_SAMP DISTINCT
    let dbl_ft = new_field_type(TypeDouble);
    for name in [
        ast::AggFuncVarPop,
        ast::AggFuncVarSamp,
        ast::AggFuncStddevPop,
        ast::AggFuncStddevSamp,
    ] {
        let desc = new_agg_func_desc(&*ctx, name, vec![column(&dbl_ft, 0)], true).unwrap();
        let t = new_tracker(1, -1);
        {
            let a = TrackedAlloc::new(Arc::clone(&t));
            let mut f = built(&ctx, &desc);
            f.set_alloc(ColAlloc::Tracked(a.clone()));
            let mut pr = f.alloc_partial_result();
            let p = agg_tester(name, TypeDouble, 1000, vec![]);
            let src = gen_src_chk(&p);
            update_each(ec, &*f, &rows(&src), &mut pr).unwrap();
            a.flush();
            assert!(
                t.bytes_consumed() > 0,
                "{name} DISTINCT: no growth accounted"
            );
            f.reset_partial_result(&mut pr);
            update_each(ec, &*f, &rows(&src), &mut pr).unwrap();
            a.flush();
            assert!(t.bytes_consumed() > 0);
        }
        assert_eq!(
            t.bytes_consumed(),
            0,
            "{name} DISTINCT: memory not returned"
        );
    }

    // 2. JSON_ARRAYAGG
    let int_ft = new_field_type(TypeLonglong);
    let desc = new_agg_func_desc(
        &*ctx,
        ast::AggFuncJsonArrayagg,
        vec![column(&int_ft, 0)],
        false,
    )
    .unwrap();
    let t = new_tracker(1, -1);
    {
        let a = TrackedAlloc::new(Arc::clone(&t));
        let mut f = built(&ctx, &desc);
        f.set_alloc(ColAlloc::Tracked(a.clone()));
        let mut pr = f.alloc_partial_result();
        let p = agg_tester(ast::AggFuncJsonArrayagg, TypeLonglong, 1000, vec![]);
        let src = gen_src_chk(&p);
        update_each(ec, &*f, &rows(&src), &mut pr).unwrap();
        a.flush();
        assert!(t.bytes_consumed() > 0, "JSON_ARRAYAGG: no growth accounted");
    }
    assert_eq!(t.bytes_consumed(), 0, "JSON_ARRAYAGG: memory not returned");

    // 3. JSON_OBJECTAGG (map table + boxed keys accounted)
    let str_ft = new_field_type(TypeString);
    let desc = new_agg_func_desc(
        &*ctx,
        ast::AggFuncJsonObjectAgg,
        vec![column(&str_ft, 0), column(&int_ft, 1)],
        false,
    )
    .unwrap();
    let t = new_tracker(1, -1);
    {
        let a = TrackedAlloc::new(Arc::clone(&t));
        let mut f = built(&ctx, &desc);
        f.set_alloc(ColAlloc::Tracked(a.clone()));
        let mut pr = f.alloc_partial_result();
        let mut chk = new_chunk_with_capacity(&[str_ft.clone(), int_ft.clone()], 1000);
        for i in 0..1000i64 {
            let key = format!("k{i:01024}");
            chk.append_string(0, key.as_bytes());
            chk.append_int64(1, i);
        }
        update_each(ec, &*f, &rows(&chk), &mut pr).unwrap();
        a.flush();
        assert!(
            t.bytes_consumed() >= 1 << 20,
            "JSON_OBJECTAGG keys not accounted: {} bytes",
            t.bytes_consumed()
        );
    }
    assert_eq!(t.bytes_consumed(), 0, "JSON_OBJECTAGG: memory not returned");
}

// TestMergePartialResult4GroupConcat (func_group_concat_test.go).
#[test]
fn merge_partial_result_group_concat() {
    let s = |v: &str| new_string_datum(v);
    test_merge_partial_result(&agg_tester(
        ast::AggFuncGroupConcat,
        TypeString,
        5,
        vec![s("0 1 2 3 4"), s("2 3 4"), s("0 1 2 3 4 2 3 4")],
    ));
}

// TestGroupConcat (func_group_concat_test.go).
#[test]
fn group_concat() {
    let s = |v: &str| new_string_datum(v);
    test_agg_func(&agg_tester(
        ast::AggFuncGroupConcat,
        TypeString,
        5,
        vec![null(), s("0 1 2 3 4")],
    ));

    let mut test2 = multi_args_agg_tester(
        ast::AggFuncGroupConcat,
        &[TypeString, TypeString],
        5,
        vec![null(), s("44 33 22 11 00")],
    );
    test2.order_by = true;
    test_multi_args_agg_func(&test2);

    // minimum GroupConcatMaxLen is 4
    let full = "44 33 22 11 00";
    for i in 4..=7usize {
        let ctx = es::new_expr_context(vec![
            es::with_eval_ctx(es::new_eval_context(vec![es::with_type_flags(
                Flags::default().with_truncate_as_warning(true),
            )])),
            es::with_group_concat_max_len(i as u64),
        ]);
        let mut t = multi_args_agg_tester(
            ast::AggFuncGroupConcat,
            &[TypeString, TypeString],
            5,
            vec![null(), s(&full[..i])],
        );
        t.order_by = true;
        test_multi_args_agg_func_with_ctx(&ctx, &t);
    }
}

// Spec §5.1 / §5.2 / §5.5 / §6.1: GROUP_CONCAT truncation error vs warning, shared truncated
// flag firing once across partial + final, GetTruncated absent on groupConcatDistinctOrder,
// and ErrInternal on ORDER BY MergePartialResult.
#[test]
fn group_concat_truncation_and_errors() {
    let str_ft = new_field_type(TypeString);
    let args = vec![column(&str_ft, 0), sep_const(",")];

    // 1. TruncateAsWarning = false -> returns ErrCutValueGroupConcat on first truncation,
    // then Ok(()) on subsequent truncations because the sentinel is already set.
    let ctx_err = es::new_expr_context(vec![
        es::with_eval_ctx(es::new_eval_context(vec![es::with_type_flags(
            Flags::default().with_truncate_as_warning(false),
        )])),
        es::with_group_concat_max_len(4),
    ]);
    let desc = new_agg_func_desc(&*ctx_err, ast::AggFuncGroupConcat, args.clone(), false).unwrap();
    let f = built(&ctx_err, &desc);
    let mut pr = f.alloc_partial_result();
    let mut chk = new_chunk_with_capacity(std::slice::from_ref(&str_ft), 2);
    chk.append_string(0, b"abc");
    chk.append_string(0, b"def");
    let ec = ctx_err.get_eval_ctx();
    let err = f
        .update_partial_result(&*ec, &EvalState::new(), &rows(&chk), &mut pr)
        .unwrap_err();
    assert!(ErrCutValueGroupConcat.equal(&err), "{err:?}");
    let dt = final_datum(&*ec, &*f, &mut pr, &desc.ret_tp);
    assert_datum_eq(&*ec, &dt, &new_string_datum("abc,"), str_ft.get_collate());
    // Second group truncates without erroring again.
    f.reset_partial_result(&mut pr);
    f.update_partial_result(&*ec, &EvalState::new(), &rows(&chk), &mut pr)
        .unwrap();

    // 2. TruncateAsWarning = true + shared truncated sentinel across partial & final:
    // generates exactly one warning across both objects.
    let eval_warn = es::new_eval_context(vec![es::with_type_flags(FlagTruncateAsWarning)]);
    let ctx_warn = es::new_expr_context(vec![
        es::with_eval_ctx(Arc::clone(&eval_warn)),
        es::with_group_concat_max_len(4),
    ]);
    let desc = new_agg_func_desc(&*ctx_warn, ast::AggFuncGroupConcat, args.clone(), false).unwrap();
    let (partial_desc, final_desc) = desc.split(&[0, 1]);
    let partial_f = built(&ctx_warn, &partial_desc);
    let mut final_f = built(&ctx_warn, &final_desc);
    let shared = partial_f
        .get_truncated()
        .expect("groupConcat has truncated");
    final_f.set_truncated(shared);
    let mut p1 = partial_f.alloc_partial_result();
    let mut p2 = partial_f.alloc_partial_result();
    let mut fpr = final_f.alloc_partial_result();
    partial_f
        .update_partial_result(&*eval_warn, &EvalState::new(), &rows(&chk), &mut p1)
        .unwrap();
    partial_f
        .update_partial_result(&*eval_warn, &EvalState::new(), &rows(&chk), &mut p2)
        .unwrap();
    final_f
        .merge_partial_result(&*eval_warn, &mut p1, &mut fpr)
        .unwrap();
    final_f
        .merge_partial_result(&*eval_warn, &mut p2, &mut fpr)
        .unwrap();
    assert_eq!(eval_warn.warning_count(), 1);
    let warns = eval_warn.copy_warnings(vec![]);
    assert!(ErrCutValueGroupConcat.equal(&warns[0].err));

    // 3. groupConcatOrder / groupConcatDistinctOrder MergePartialResult -> ErrInternal;
    // groupConcatDistinctOrder omits GetTruncated (matching Go).
    for distinct in [false, true] {
        let mut desc =
            new_agg_func_desc(&*ctx_warn, ast::AggFuncGroupConcat, args.clone(), distinct).unwrap();
        desc.order_by_items = vec![ByItems {
            expr: Arc::clone(&args[0]),
            desc: false,
        }];
        let f = built(&ctx_warn, &desc);
        if distinct {
            assert!(f.get_truncated().is_none());
        } else {
            assert!(f.get_truncated().is_some());
        }
        let mut a = f.alloc_partial_result();
        let mut b = f.alloc_partial_result();
        let err = f
            .merge_partial_result(&*eval_warn, &mut a, &mut b)
            .unwrap_err();
        assert!(plannererrors::ErrInternal.equal(&err), "{err:?}");
    }
}

// TestWriteTime (func_count_test.go).
#[test]
fn write_time_fills_all_bytes() {
    let ec = es::new_eval_context(vec![]);
    let (tt, err) = parse_date(&ec.type_ctx(), b"2020-11-11");
    err.unwrap();
    let mut buf = [255u8; 16];
    write_time(&mut buf, tt);
    for (i, &b) in buf.iter().enumerate() {
        assert_ne!(b, 255, "byte {i} was not written");
    }
}

// TestMemCount (DISTINCT cases) and TestMemGroupConcat (all 4 variants) as A1 property tests
// (spec §5.3): growing partial results account through ColAlloc / TrackedAlloc and release on drop.
#[test]
fn count_distinct_and_group_concat_memory_accounted() {
    let ctx = es::new_expr_context(vec![]);
    let ec = ctx.get_eval_ctx();
    let ec: &dyn EvalContext = &*ec;

    // 1. COUNT(DISTINCT) single-arg (every specialized type + fallback Date/JSON) and multi-arg.
    for tp in ALL_TYPES {
        let ft = new_field_type(tp);
        let desc = new_agg_func_desc(&*ctx, ast::AggFuncCount, vec![column(&ft, 0)], true).unwrap();
        let t = new_tracker(1, -1);
        {
            let a = TrackedAlloc::new(Arc::clone(&t));
            let mut f = built(&ctx, &desc);
            f.set_alloc(ColAlloc::Tracked(a.clone()));
            let mut pr = f.alloc_partial_result();
            let p = agg_tester(ast::AggFuncCount, tp, 500, vec![]);
            let src = gen_src_chk(&p);
            update_each(ec, &*f, &rows(&src), &mut pr).unwrap();
            a.flush();
            assert!(
                t.bytes_consumed() > 0,
                "COUNT(DISTINCT) tp={tp}: no growth accounted"
            );
            f.reset_partial_result(&mut pr);
            update_each(ec, &*f, &rows(&src), &mut pr).unwrap();
            a.flush();
            assert!(t.bytes_consumed() > 0);
        }
        assert_eq!(
            t.bytes_consumed(),
            0,
            "COUNT(DISTINCT) tp={tp}: memory not returned"
        );
    }

    // 2. GROUP_CONCAT (plain, DISTINCT, ORDER BY, DISTINCT ORDER BY).
    let str_ft = new_field_type(TypeString);
    let ctx_gc = es::new_expr_context(vec![es::with_group_concat_max_len(1024 * 1024)]);
    let ec_gc = ctx_gc.get_eval_ctx();
    for (distinct, order_by) in [(false, false), (true, false), (false, true), (true, true)] {
        let args = vec![column(&str_ft, 0), column(&str_ft, 1), sep_const(SEPARATOR)];
        let mut desc =
            new_agg_func_desc(&*ctx_gc, ast::AggFuncGroupConcat, args.clone(), distinct).unwrap();
        if order_by {
            desc.order_by_items = vec![ByItems {
                expr: Arc::clone(&args[0]),
                desc: true,
            }];
        }
        let t = new_tracker(1, -1);
        {
            let a = TrackedAlloc::new(Arc::clone(&t));
            let mut f = built(&ctx_gc, &desc);
            f.set_alloc(ColAlloc::Tracked(a.clone()));
            let mut pr = f.alloc_partial_result();
            let p = multi_args_agg_tester(
                ast::AggFuncGroupConcat,
                &[TypeString, TypeString],
                500,
                vec![],
            );
            let src = gen_multi_src_chk(&p);
            update_each(&*ec_gc, &*f, &rows(&src), &mut pr).unwrap();
            a.flush();
            assert!(
                t.bytes_consumed() > 0,
                "GROUP_CONCAT distinct={distinct} order_by={order_by}: no growth accounted"
            );
            f.reset_partial_result(&mut pr);
            update_each(&*ec_gc, &*f, &rows(&src), &mut pr).unwrap();
            a.flush();
            assert!(t.bytes_consumed() > 0);
        }
        assert_eq!(
            t.bytes_consumed(),
            0,
            "GROUP_CONCAT distinct={distinct} order_by={order_by}: memory not returned"
        );
    }
}

// TestPercentile (func_percentile_test.go).
#[test]
fn percentile_agg_func() {
    for p in [
        agg_tester(
            ast::AggFuncApproxPercentile,
            TypeLonglong,
            5,
            vec![null(), new_int_datum(2)],
        ),
        agg_tester(
            ast::AggFuncApproxPercentile,
            TypeFloat,
            5,
            vec![null(), new_float32_datum(2.0)],
        ),
        agg_tester(
            ast::AggFuncApproxPercentile,
            TypeDouble,
            5,
            vec![null(), new_float64_datum(2.0)],
        ),
        agg_tester(
            ast::AggFuncApproxPercentile,
            TypeNewDecimal,
            5,
            vec![null(), dec(2)],
        ),
        agg_tester(
            ast::AggFuncApproxPercentile,
            TypeDate,
            5,
            vec![null(), new_time_datum(time_from_days(367))],
        ),
        agg_tester(
            ast::AggFuncApproxPercentile,
            TypeDuration,
            5,
            vec![
                null(),
                new_duration_datum(Duration {
                    duration: 2,
                    fsp: 0,
                }),
            ],
        ),
    ] {
        test_agg_func(&p);
    }
}

// TestFix26807 (func_percentile_test.go): verifies k = Ceil(100/100 * N) selects the max element,
// exercising both seeded and production (thread_rng) paths.
#[test]
fn percentile_fix_26807() {
    struct TestSlice(Vec<isize>);
    impl selection::Interface for TestSlice {
        fn len(&self) -> isize {
            self.0.len() as isize
        }
        fn less(&self, i: isize, j: isize) -> bool {
            self.0[i as usize] < self.0[j as usize]
        }
        fn swap(&mut self, i: isize, j: isize) {
            self.0.swap(i as usize, j as usize);
        }
    }

    let want: isize = 28;
    let mut data = TestSlice((1..=want).collect());
    for i in 0..10u64 {
        let index = percentile(&mut data, 100, Some(i));
        assert_eq!(data.0[index as usize], want);
        let index_prod = percentile(&mut data, 100, None);
        assert_eq!(data.0[index_prod as usize], want);
    }
}

// TestFix40463 (func_percentile_test.go): ENUM / SET with EnumSetAsIntFlag map to basePercentile
// (returning NULL) instead of percentileOriginal4Int.
#[test]
fn percentile_fix_40463() {
    for tp in [TypeEnum, TypeSet] {
        let mut p = agg_tester(ast::AggFuncApproxPercentile, tp, 5, vec![null(), null()]);
        p.data_type.add_flag(mysql::EnumSetAsIntFlag);
        test_agg_func(&p);
    }
}

// APPROX_PERCENTILE merge + A1 memory accounting across all five typed implementations and
// BasePercentile, including split-pair compatibility (Partial1 -> Final).
#[test]
fn approx_percentile_merge_and_mem() {
    let ctx = es::new_expr_context(vec![]);
    let ec = ctx.get_eval_ctx();
    let ec: &dyn EvalContext = &*ec;
    let pct = Expr::Constant(Constant::new(
        new_int_datum(50),
        new_field_type(mysql::TypeLong),
    ))
    .into_ref();

    for tp in [
        TypeLonglong,
        TypeDouble,
        TypeNewDecimal,
        TypeDate,
        TypeDuration,
    ] {
        let ft = new_field_type(tp);
        let desc = new_agg_func_desc(
            &*ctx,
            ast::AggFuncApproxPercentile,
            vec![column(&ft, 0), Arc::clone(&pct)],
            false,
        )
        .unwrap();
        let (partial_desc, final_desc) = desc.split(&[0, 1]);

        let t = new_tracker(1, -1);
        {
            let a = TrackedAlloc::new(Arc::clone(&t));
            let mut pf = built(&ctx, &partial_desc);
            let mut ff = built(&ctx, &final_desc);
            pf.set_alloc(ColAlloc::Tracked(a.clone()));
            ff.set_alloc(ColAlloc::Tracked(a.clone()));
            ff.set_rng_seed_for_test(99);

            let mut ppr1 = pf.alloc_partial_result();
            let mut ppr2 = pf.alloc_partial_result();
            let mut fpr = ff.alloc_partial_result();

            let src = gen_src_chk(&agg_tester(ast::AggFuncApproxPercentile, tp, 200, vec![]));
            let rs = rows(&src);
            update_each(ec, &*pf, &rs[..100], &mut ppr1).unwrap();
            update_each(ec, &*pf, &rs[100..], &mut ppr2).unwrap();
            a.flush();
            assert!(
                t.bytes_consumed() > 0,
                "APPROX_PERCENTILE tp={tp}: buffer growth not accounted"
            );

            ff.merge_partial_result(ec, &mut ppr1, &mut fpr).unwrap();
            ff.merge_partial_result(ec, &mut ppr2, &mut fpr).unwrap();
            let dt = final_datum(ec, &*ff, &mut fpr, &desc.ret_tp);
            assert!(!dt.is_null(), "APPROX_PERCENTILE tp={tp}: expected value");

            ff.reset_partial_result(&mut fpr);
            let dt_empty = final_datum(ec, &*ff, &mut fpr, &desc.ret_tp);
            assert!(dt_empty.is_null());
        }
        assert_eq!(
            t.bytes_consumed(),
            0,
            "APPROX_PERCENTILE tp={tp}: memory not returned on drop"
        );
    }
}

// Maintainer decision in #393 / upstream-pitfall #394 (Constitution art. 1.2 & art. 9.3):
// APPROX_COUNT_DISTINCT uses a separate Go-raw encoding path (`eval_and_encode_go_raw`) so
// ruling R23 (#390) canonicalization does NOT leak into APPROX_COUNT_DISTINCT.
#[test]
fn approx_count_distinct_keeps_go_raw_encoding() {
    let ctx = es::new_expr_context(vec![]);
    let ec = ctx.get_eval_ctx();
    let ec: &dyn EvalContext = &*ec;
    let st = EvalState::new();

    // 1. Float +0.0 vs -0.0: APPROX_COUNT_DISTINCT hashes raw IEEE-754 bits -> 2 (Go v8.5.8).
    let dbl_ft = new_field_type(TypeDouble);
    let mut chk_f = new_chunk_with_capacity(std::slice::from_ref(&dbl_ft), 2);
    chk_f.append_float64(0, 0.0);
    chk_f.append_float64(0, -0.0);
    let desc_f = new_agg_func_desc(
        &*ctx,
        ast::AggFuncApproxCountDistinct,
        vec![column(&dbl_ft, 0)],
        false,
    )
    .unwrap();
    let f = built(&ctx, &desc_f);
    let mut pr = f.alloc_partial_result();
    f.update_partial_result(ec, &st, &rows(&chk_f), &mut pr)
        .unwrap();
    let dt = final_datum(ec, &*f, &mut pr, &desc_f.ret_tp);
    assert_datum_eq(ec, &dt, &new_int_datum(2), "binary");

    // 2. Time with equal CoreTime and differing Type / Fsp: APPROX_COUNT_DISTINCT hashes
    // full 16-byte WriteTime output -> 3 distinct entries (Date/0, Datetime/0, Datetime/3).
    let (t_date, err) = parse_date(&ec.type_ctx(), b"2020-01-01");
    err.unwrap();
    let t_dt0 = tidb_types::time::new_time(t_date.core_time(), mysql::TypeDatetime, 0);
    let t_dt3 = tidb_types::time::new_time(t_date.core_time(), mysql::TypeDatetime, 3);
    let mut dt_ft = new_field_type(mysql::TypeDatetime);
    dt_ft.set_decimal(3);
    let mut chk_t = new_chunk_with_capacity(std::slice::from_ref(&dt_ft), 3);
    chk_t.append_time(0, t_date);
    chk_t.append_time(0, t_dt0);
    chk_t.append_time(0, t_dt3);
    let desc_t = new_agg_func_desc(
        &*ctx,
        ast::AggFuncApproxCountDistinct,
        vec![column(&dt_ft, 0)],
        false,
    )
    .unwrap();
    let ft_agg = built(&ctx, &desc_t);
    let mut pr_t = ft_agg.alloc_partial_result();
    ft_agg
        .update_partial_result(ec, &st, &rows(&chk_t), &mut pr_t)
        .unwrap();
    let dt = final_datum(ec, &*ft_agg, &mut pr_t, &desc_t.ret_tp);
    assert_datum_eq(ec, &dt, &new_int_datum(3), "binary");

    // 3. Duration with equal nanoseconds and differing Fsp (0 vs 3): Go raw encoding writes
    // 16 bytes (Duration + Fsp), whereas R23 `eval_and_encode` writes 8 bytes (Duration only).
    // (Chunk columns store only the int64 nanoseconds and derive Fsp from FieldType.decimal, so
    // per-value Fsp differences arise from expressions such as Constant / scalar functions.)
    let mut dur_ft0 = new_field_type(TypeDuration);
    dur_ft0.set_decimal(0);
    let mut dur_ft3 = new_field_type(TypeDuration);
    dur_ft3.set_decimal(3);
    let c_dur0 = Expr::Constant(Constant::new(
        new_duration_datum(Duration {
            duration: 1_000_000_000,
            fsp: 0,
        }),
        dur_ft0,
    ))
    .into_ref();
    let c_dur3 = Expr::Constant(Constant::new(
        new_duration_datum(Duration {
            duration: 1_000_000_000,
            fsp: 3,
        }),
        dur_ft3,
    ))
    .into_ref();
    let collator = get_collator("binary");
    let mut buf = [0u8; 16];
    let (raw0, null0) =
        eval_and_encode_go_raw(ec, &st, &c_dur0, collator, Row::default(), &mut buf, vec![])
            .unwrap();
    let (raw3, null3) =
        eval_and_encode_go_raw(ec, &st, &c_dur3, collator, Row::default(), &mut buf, vec![])
            .unwrap();
    assert!(!null0 && !null3);
    assert_eq!(raw0.len(), 16);
    assert_eq!(raw3.len(), 16);
    assert_ne!(
        raw0, raw3,
        "Go raw encoding for APPROX_COUNT_DISTINCT must preserve Duration.Fsp (isolated from R23)"
    );
    // And verify APPROX_COUNT_DISTINCT Partial1 hashes those exact 16-byte buffers differently.
    let mut p_d0 = new_partial_result_4_approx_count_distinct();
    p_d0.insert_hash64(farm::hash64(&raw0));
    let mut p_d3 = new_partial_result_4_approx_count_distinct();
    p_d3.insert_hash64(farm::hash64(&raw3));
    assert_ne!(p_d0.serialize(), p_d3.serialize());
}

// APPROX_COUNT_DISTINCT: resize, skip_degree > 0 (> 65536 distinct items), has_zero,
// read_and_merge error ("too large size degree"), Partial1/Partial2/Final wire round-trip,
// and A1 TrackedAlloc memory accounting.
#[test]
fn approx_count_distinct_sketch_modes_and_mem() {
    let ctx = es::new_expr_context(vec![]);
    let ec = ctx.get_eval_ctx();
    let ec: &dyn EvalContext = &*ec;
    let st = EvalState::new();

    // 1. Cross uniquesHashMaxSize (65536) to trigger skip_degree > 0, rehash, and fixed_size.
    let mut sketch = new_partial_result_4_approx_count_distinct();
    sketch.insert_hash64(0); // exercises has_zero branch
    for i in 0..70_000u64 {
        sketch.insert_hash64(farm::hash64(&i.to_le_bytes()));
    }
    let est = sketch.fixed_size();
    assert!(
        (65_000..=75_000).contains(&est),
        "BJKST estimate {est} out of expected range around 70,000"
    );
    let bytes = sketch.serialize();
    assert!(bytes[0] > 0, "skip_degree should have incremented");

    // 2. Round-trip through Partial2 -> Final (wire read_and_merge + Serialize).
    let str_ft = new_field_type(TypeString);
    let mut p2_desc = new_agg_func_desc(
        &*ctx,
        ast::AggFuncApproxCountDistinct,
        vec![column(&str_ft, 0)],
        false,
    )
    .unwrap();
    p2_desc.mode = AggFunctionMode::Partial2Mode;
    let mut fin_desc = new_agg_func_desc(
        &*ctx,
        ast::AggFuncApproxCountDistinct,
        vec![column(&str_ft, 0)],
        false,
    )
    .unwrap();
    fin_desc.mode = AggFunctionMode::FinalMode;

    let p2 = built(&ctx, &p2_desc);
    let fin = built(&ctx, &fin_desc);
    let mut p2_pr = p2.alloc_partial_result();
    let mut chk_in = new_chunk_with_capacity(std::slice::from_ref(&str_ft), 1);
    chk_in.append_bytes(0, &bytes);
    p2.update_partial_result(ec, &st, &rows(&chk_in), &mut p2_pr)
        .unwrap();
    let p2_out = final_datum(ec, &*p2, &mut p2_pr, &str_ft);

    let mut fin_pr = fin.alloc_partial_result();
    let mut chk_p2 = new_chunk_with_capacity(std::slice::from_ref(&str_ft), 1);
    chk_p2.append_bytes(0, p2_out.get_bytes());
    fin.update_partial_result(ec, &st, &rows(&chk_p2), &mut fin_pr)
        .unwrap();
    let fin_out = final_datum(ec, &*fin, &mut fin_pr, &fin_desc.ret_tp);
    assert_datum_eq(ec, &fin_out, &new_int_datum(est as i64), "binary");

    // 3. read_and_merge rejects rhsSize > uniquesHashMaxSize (65536).
    let mut bad = vec![0u8]; // skip_degree = 0
    tidb_codec::number::encode_uvarint(&mut bad, 65537);
    let mut chk_bad = new_chunk_with_capacity(std::slice::from_ref(&str_ft), 1);
    chk_bad.append_bytes(0, &bad);
    let mut bad_pr = fin.alloc_partial_result();
    let err = fin
        .update_partial_result(ec, &st, &rows(&chk_bad), &mut bad_pr)
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "Cannot read partialResult4ApproxCountDistinct: too large size degree"
    );

    // 4. A1 TrackedAlloc memory accounting across resizes, reset, and drop.
    let int_ft = new_field_type(TypeLonglong);
    let desc = new_agg_func_desc(
        &*ctx,
        ast::AggFuncApproxCountDistinct,
        vec![column(&int_ft, 0)],
        false,
    )
    .unwrap();
    let t = new_tracker(1, -1);
    {
        let a = TrackedAlloc::new(Arc::clone(&t));
        let mut f = built(&ctx, &desc);
        f.set_alloc(ColAlloc::Tracked(a.clone()));
        let mut pr = f.alloc_partial_result();
        a.flush();
        let initial = t.bytes_consumed();
        assert!(initial > 0, "initial hash table buffer must be accounted");
        let mut chk = new_chunk_with_capacity(std::slice::from_ref(&int_ft), 200);
        for i in 0..200i64 {
            chk.append_int64(0, i);
        }
        f.update_partial_result(ec, &st, &rows(&chk), &mut pr)
            .unwrap();
        a.flush();
        let grown = t.bytes_consumed();
        assert!(
            grown > initial,
            "hash table resize must grow tracker ({grown} <= {initial})"
        );
        f.reset_partial_result(&mut pr);
        a.flush();
        assert_eq!(
            t.bytes_consumed(),
            initial,
            "reset must shrink buffer back to initial size degree"
        );
    }
    assert_eq!(
        t.bytes_consumed(),
        0,
        "APPROX_COUNT_DISTINCT memory not returned on drop"
    );
}

// Ruling R23 (#390, constitution art. 9.3): COUNT(DISTINCT) deduplicates equal values
// consistently across single-argument and multi-argument paths:
// - {+0.0, -0.0} counts as 1 in both COUNT(DISTINCT x) and COUNT(DISTINCT x, 1).
// - Duration values with equal nanoseconds and differing Fsp (e.g. TIME(str) on '00:00:01' vs
//   '00:00:01.000') encode to the same key and count as 1 in both single- and multi-arg paths.
// - Time values with equal CoreTime and differing Type or Fsp (e.g. COALESCE(date, datetime) or
//   TIMESTAMP(str)) encode to the same key and count as 1.
#[test]
fn count_distinct_canonical_keys() {
    let ctx = es::new_expr_context(vec![]);
    let ec = ctx.get_eval_ctx();
    let ec: &dyn EvalContext = &*ec;
    let st = EvalState::new();

    // 1. {+0.0, -0.0}: single-arg COUNT(DISTINCT x) and multi-arg COUNT(DISTINCT x, 1) both == 1.
    let dbl_ft = new_field_type(TypeDouble);
    let int_ft = new_field_type(TypeLonglong);
    let mut chk = new_chunk_with_capacity(&[dbl_ft.clone(), int_ft.clone()], 2);
    chk.append_float64(0, 0.0);
    chk.append_int64(1, 1);
    chk.append_float64(0, -0.0);
    chk.append_int64(1, 1);

    for args in [
        vec![column(&dbl_ft, 0)],
        vec![column(&dbl_ft, 0), column(&int_ft, 1)],
    ] {
        let desc = new_agg_func_desc(&*ctx, ast::AggFuncCount, args, true).unwrap();
        let f = built(&ctx, &desc);
        let mut pr = f.alloc_partial_result();
        f.update_partial_result(ec, &st, &rows(&chk), &mut pr)
            .unwrap();
        let dt = final_datum(ec, &*f, &mut pr, &desc.ret_tp);
        assert_datum_eq(ec, &dt, &new_int_datum(1), "binary");
    }

    // 2. Duration with equal value and differing Fsp (0 vs 3): both single-arg and multi-arg
    // produce identical encoded keys and count as 1.
    let mut dur_ft0 = new_field_type(TypeDuration);
    dur_ft0.set_decimal(0);
    let mut dur_ft3 = new_field_type(TypeDuration);
    dur_ft3.set_decimal(3);
    let dur_val = 1_000_000_000i64; // 00:00:01
    let c_dur0 = Expr::Constant(Constant::new(
        new_duration_datum(Duration {
            duration: dur_val,
            fsp: 0,
        }),
        dur_ft0,
    ))
    .into_ref();
    let c_dur3 = Expr::Constant(Constant::new(
        new_duration_datum(Duration {
            duration: dur_val,
            fsp: 3,
        }),
        dur_ft3,
    ))
    .into_ref();
    let collator = get_collator("binary");
    let mut buf = [0u8; 16];
    let (k_dur0, null0) =
        eval_and_encode(ec, &st, &c_dur0, collator, Row::default(), &mut buf, vec![]).unwrap();
    let (k_dur3, null3) =
        eval_and_encode(ec, &st, &c_dur3, collator, Row::default(), &mut buf, vec![]).unwrap();
    assert!(!null0 && !null3);
    assert_eq!(
        k_dur0, k_dur3,
        "Duration with differing Fsp must encode to the same key (R23)"
    );

    // 3. Time with equal CoreTime and differing Type (Date vs Datetime) and Fsp (0 vs 3):
    // encodes to the same key under R23.
    let (t_date, err) = parse_date(&ec.type_ctx(), b"2020-01-01");
    err.unwrap();
    let t_dt0 = tidb_types::time::new_time(t_date.core_time(), mysql::TypeDatetime, 0);
    let t_dt3 = tidb_types::time::new_time(t_date.core_time(), mysql::TypeDatetime, 3);
    let c_date = Expr::Constant(Constant::new(
        new_time_datum(t_date),
        new_field_type(TypeDate),
    ))
    .into_ref();
    let c_dt0 = Expr::Constant(Constant::new(
        new_time_datum(t_dt0),
        new_field_type(mysql::TypeDatetime),
    ))
    .into_ref();
    let mut dt3_ft = new_field_type(mysql::TypeDatetime);
    dt3_ft.set_decimal(3);
    let c_dt3 = Expr::Constant(Constant::new(new_time_datum(t_dt3), dt3_ft)).into_ref();

    let (k_date, _) =
        eval_and_encode(ec, &st, &c_date, collator, Row::default(), &mut buf, vec![]).unwrap();
    let (k_dt0, _) =
        eval_and_encode(ec, &st, &c_dt0, collator, Row::default(), &mut buf, vec![]).unwrap();
    let (k_dt3, _) =
        eval_and_encode(ec, &st, &c_dt3, collator, Row::default(), &mut buf, vec![]).unwrap();
    assert_eq!(
        k_date, k_dt0,
        "Date and Datetime of same instant must match (R23)"
    );
    assert_eq!(k_dt0, k_dt3, "Datetime with differing Fsp must match (R23)");
}

// A1 payload accounting (spec §5.3): large payloads in JSON_ARRAYAGG / JSON_OBJECTAGG
// (String, Opaque VARBINARY, BinaryJSON) and GROUP_CONCAT ORDER BY (SortRow.by_items Datum
// payloads) must be accounted on the operator tracker, trigger memory-quota checkpoints, and
// return cleanly to 0 on overwrite, eviction, reset, and drop.
#[test]
fn json_agg_and_group_concat_order_payloads_accounted() {
    use tidb_memory::action::PanicOnExceed;
    use tidb_parser::gocharset;
    use tidb_types::json::parse_binary_json_from_string;

    let ctx = es::new_expr_context(vec![]);
    let ec = ctx.get_eval_ctx();
    let ec: &dyn EvalContext = &*ec;
    let n: usize = 200;
    let payload_len: usize = 1024;
    let expected_min = (n * payload_len) as i64;

    // 1. JSON_ARRAYAGG across String, Opaque (VARBINARY), and BinaryJSON payloads.
    let mut vb_ft = new_field_type(mysql::TypeVarString);
    vb_ft.set_charset(gocharset::CharsetBin);
    vb_ft.set_collate(gocharset::CollationBin);
    let str_ft = new_field_type(mysql::TypeVarString);
    let json_ft = new_field_type(TypeJSON);

    for (label, ft) in [
        ("string", str_ft.clone()),
        ("opaque_varbinary", vb_ft.clone()),
        ("binary_json", json_ft.clone()),
    ] {
        let mut chk = new_chunk_with_capacity(std::slice::from_ref(&ft), n);
        for i in 0..n {
            let body = format!("{i:04}{}", "a".repeat(payload_len - 4));
            if ft.get_type() == TypeJSON {
                let bj = parse_binary_json_from_string(format!("\"{body}\"").as_bytes()).unwrap();
                chk.append_json(0, &bj);
            } else {
                chk.append_bytes(0, body.as_bytes());
            }
        }
        let desc = new_agg_func_desc(&*ctx, ast::AggFuncJsonArrayagg, vec![column(&ft, 0)], false)
            .unwrap();

        let t = new_tracker(1, -1);
        t.set_bytes_limit(expected_min / 2);
        t.set_action_on_exceed(Some(PanicOnExceed::new(
            Some(Arc::new(tidb_memory::SqlKiller::default())),
            0,
        )));
        {
            let a = TrackedAlloc::new(Arc::clone(&t));
            let mut f = built(&ctx, &desc);
            f.set_alloc(ColAlloc::Tracked(a.clone()));
            let mut pr = f.alloc_partial_result();
            update_each(ec, &*f, &rows(&chk), &mut pr).unwrap();
            a.flush();
            assert!(
                t.bytes_consumed() >= expected_min,
                "JSON_ARRAYAGG {label}: tracked {} < {expected_min}",
                t.bytes_consumed()
            );
            assert!(
                t.checkpoint().is_err(),
                "JSON_ARRAYAGG {label}: quota checkpoint must fire"
            );

            // Merge into a second tracked destination and verify dst accounts the merged payloads.
            let t_dst = new_tracker(2, -1);
            {
                let a_dst = TrackedAlloc::new(Arc::clone(&t_dst));
                let mut f_dst = built(&ctx, &desc);
                f_dst.set_alloc(ColAlloc::Tracked(a_dst.clone()));
                let mut pr_dst = f_dst.alloc_partial_result();
                f_dst
                    .merge_partial_result(ec, &mut pr, &mut pr_dst)
                    .unwrap();
                a_dst.flush();
                assert!(
                    t_dst.bytes_consumed() >= expected_min,
                    "JSON_ARRAYAGG {label} merge: tracked {} < {expected_min}",
                    t_dst.bytes_consumed()
                );
            }
            assert_eq!(t_dst.bytes_consumed(), 0);

            f.reset_partial_result(&mut pr);
            a.flush();
            assert!(
                t.bytes_consumed() < expected_min,
                "JSON_ARRAYAGG {label}: reset must release payload bytes"
            );
        }
        assert_eq!(
            t.bytes_consumed(),
            0,
            "JSON_ARRAYAGG {label}: memory not returned on drop"
        );
    }

    // 2. JSON_OBJECTAGG across String, Opaque, BinaryJSON + duplicate-key overwrite release.
    for (label, val_ft) in [
        ("string", str_ft.clone()),
        ("opaque_varbinary", vb_ft.clone()),
        ("binary_json", json_ft.clone()),
    ] {
        let mut chk_big = new_chunk_with_capacity(&[str_ft.clone(), val_ft.clone()], n);
        let mut chk_small = new_chunk_with_capacity(&[str_ft.clone(), val_ft.clone()], n);
        for i in 0..n {
            let k = format!("k{i:04}");
            let big = format!("{i:04}{}", "b".repeat(payload_len - 4));
            chk_big.append_string(0, k.as_bytes());
            chk_small.append_string(0, k.as_bytes());
            if val_ft.get_type() == TypeJSON {
                chk_big.append_json(
                    1,
                    &parse_binary_json_from_string(format!("\"{big}\"").as_bytes()).unwrap(),
                );
                chk_small.append_json(1, &parse_binary_json_from_string(b"\"s\"").unwrap());
            } else {
                chk_big.append_bytes(1, big.as_bytes());
                chk_small.append_bytes(1, b"s");
            }
        }
        let desc = new_agg_func_desc(
            &*ctx,
            ast::AggFuncJsonObjectAgg,
            vec![column(&str_ft, 0), column(&val_ft, 1)],
            false,
        )
        .unwrap();

        let t = new_tracker(1, -1);
        t.set_bytes_limit(expected_min / 2);
        t.set_action_on_exceed(Some(PanicOnExceed::new(
            Some(Arc::new(tidb_memory::SqlKiller::default())),
            0,
        )));
        {
            let a = TrackedAlloc::new(Arc::clone(&t));
            let mut f = built(&ctx, &desc);
            f.set_alloc(ColAlloc::Tracked(a.clone()));
            let mut pr = f.alloc_partial_result();
            update_each(ec, &*f, &rows(&chk_big), &mut pr).unwrap();
            a.flush();
            let peak = t.bytes_consumed();
            assert!(
                peak >= expected_min,
                "JSON_OBJECTAGG {label}: tracked {peak} < {expected_min}"
            );
            assert!(
                t.checkpoint().is_err(),
                "JSON_OBJECTAGG {label}: quota checkpoint must fire"
            );

            // Overwriting every key with a tiny value must release the old payloads.
            update_each(ec, &*f, &rows(&chk_small), &mut pr).unwrap();
            a.flush();
            assert!(
                t.bytes_consumed() < peak / 2,
                "JSON_OBJECTAGG {label}: overwrite did not release old payloads (peak={peak}, now={})",
                t.bytes_consumed()
            );
        }
        assert_eq!(
            t.bytes_consumed(),
            0,
            "JSON_OBJECTAGG {label}: memory not returned on drop"
        );
    }

    // 3. GROUP_CONCAT(a ORDER BY long_text): SortRow.by_items payloads tracked & released on
    // eviction, reset, and drop (both plain ORDER BY and DISTINCT ORDER BY).
    let gc_n: usize = 100;
    let by_len: usize = 2048;
    let gc_expected_min = (gc_n * by_len) as i64;
    let ctx_gc = es::new_expr_context(vec![es::with_group_concat_max_len(1024 * 1024)]);
    let ec_gc = ctx_gc.get_eval_ctx();

    let mut chk_gc = new_chunk_with_capacity(&[str_ft.clone(), str_ft.clone()], gc_n);
    for i in 0..gc_n {
        chk_gc.append_string(0, format!("v{i:03}").as_bytes());
        chk_gc.append_string(1, format!("{i:04}{}", "z".repeat(by_len - 4)).as_bytes());
    }

    for distinct in [false, true] {
        let args = vec![column(&str_ft, 0), sep_const(SEPARATOR)];
        let mut desc =
            new_agg_func_desc(&*ctx_gc, ast::AggFuncGroupConcat, args, distinct).unwrap();
        desc.order_by_items = vec![ByItems {
            expr: column(&str_ft, 1),
            desc: true,
        }];

        let t = new_tracker(1, -1);
        t.set_bytes_limit(gc_expected_min / 2);
        t.set_action_on_exceed(Some(PanicOnExceed::new(
            Some(Arc::new(tidb_memory::SqlKiller::default())),
            0,
        )));
        {
            let a = TrackedAlloc::new(Arc::clone(&t));
            let mut f = built(&ctx_gc, &desc);
            f.set_alloc(ColAlloc::Tracked(a.clone()));
            let mut pr = f.alloc_partial_result();
            update_each(&*ec_gc, &*f, &rows(&chk_gc), &mut pr).unwrap();
            a.flush();
            assert!(
                t.bytes_consumed() >= gc_expected_min,
                "GROUP_CONCAT ORDER BY distinct={distinct}: tracked {} < {gc_expected_min}",
                t.bytes_consumed()
            );
            assert!(
                t.checkpoint().is_err(),
                "GROUP_CONCAT ORDER BY distinct={distinct}: quota checkpoint must fire"
            );

            f.reset_partial_result(&mut pr);
            a.flush();
            assert!(
                t.bytes_consumed() < gc_expected_min / 4,
                "GROUP_CONCAT ORDER BY distinct={distinct}: reset must release by_items payloads (now={})",
                t.bytes_consumed()
            );
        }
        assert_eq!(
            t.bytes_consumed(),
            0,
            "GROUP_CONCAT ORDER BY distinct={distinct}: memory not returned on drop"
        );
    }

    // Also verify heap eviction under a tight group_concat_max_len drops evicted by_items payloads.
    let ctx_tight = es::new_expr_context(vec![
        es::with_eval_ctx(es::new_eval_context(vec![es::with_type_flags(
            Flags::default().with_truncate_as_warning(true),
        )])),
        es::with_group_concat_max_len(16),
    ]);
    let ec_tight = ctx_tight.get_eval_ctx();
    let args = vec![column(&str_ft, 0), sep_const(SEPARATOR)];
    let mut desc = new_agg_func_desc(&*ctx_tight, ast::AggFuncGroupConcat, args, false).unwrap();
    desc.order_by_items = vec![ByItems {
        expr: column(&str_ft, 1),
        desc: true,
    }];
    let t = new_tracker(1, -1);
    {
        let a = TrackedAlloc::new(Arc::clone(&t));
        let mut f = built(&ctx_tight, &desc);
        f.set_alloc(ColAlloc::Tracked(a.clone()));
        let mut pr = f.alloc_partial_result();
        update_each(&*ec_tight, &*f, &rows(&chk_gc), &mut pr).unwrap();
        a.flush();
        // Only a few rows fit in max_len=16, so tracked bytes must stay far below all 100 rows.
        assert!(t.bytes_consumed() > 0);
        assert!(
            t.bytes_consumed() < gc_expected_min / 4,
            "evicted by_items payloads were not released: {}",
            t.bytes_consumed()
        );
    }
    assert_eq!(t.bytes_consumed(), 0);
}

// Ruling R24 (#392, constitution art. 9.3): get_real_json_value copies Opaque bytes into owned
// ColAlloc storage instead of aliasing the child chunk, so reusing a child chunk across multiple
// batches in one group never corrupts earlier VARBINARY entries in JSON_ARRAYAGG / JSON_OBJECTAGG.
#[test]
fn json_agg_opaque_survives_chunk_reuse() {
    use tidb_parser::gocharset;
    use tidb_types::json::parse_binary_json_from_string;

    let ctx = es::new_expr_context(vec![]);
    let ec = ctx.get_eval_ctx();
    let ec: &dyn EvalContext = &*ec;
    let st = EvalState::new();

    let mut vb_ft = new_field_type(mysql::TypeVarString);
    vb_ft.set_charset(gocharset::CharsetBin);
    vb_ft.set_collate(gocharset::CollationBin);
    vb_ft.set_flen(16);
    let key_ft = new_field_type(mysql::TypeVarString);

    let batches: [&[(&[u8], &[u8])]; 3] = [
        &[(b"k0", b"AAAA"), (b"k1", b"BBBB")],
        &[(b"k2", b"CCCC"), (b"k3", b"DDDD")],
        &[(b"k4", b"XXXX"), (b"k5", b"YYYY")],
    ];

    // 1. JSON_ARRAYAGG over a single Chunk reset and overwritten 3 times.
    let arr_desc = new_agg_func_desc(
        &*ctx,
        ast::AggFuncJsonArrayagg,
        vec![column(&vb_ft, 1)],
        false,
    )
    .unwrap();
    let arr_f = built(&ctx, &arr_desc);
    let mut arr_pr = arr_f.alloc_partial_result();

    // 2. JSON_OBJECTAGG over the same reused Chunk.
    let obj_desc = new_agg_func_desc(
        &*ctx,
        ast::AggFuncJsonObjectAgg,
        vec![column(&key_ft, 0), column(&vb_ft, 1)],
        false,
    )
    .unwrap();
    let obj_f = built(&ctx, &obj_desc);
    let mut obj_pr = obj_f.alloc_partial_result();

    let mut reused = new_chunk_with_capacity(&[key_ft.clone(), vb_ft.clone()], 2);
    for batch in batches {
        reused.reset();
        for &(k, v) in batch {
            reused.append_string(0, k);
            reused.append_bytes(1, v);
        }
        let rs = rows(&reused);
        arr_f
            .update_partial_result(ec, &st, &rs, &mut arr_pr)
            .unwrap();
        obj_f
            .update_partial_result(ec, &st, &rs, &mut obj_pr)
            .unwrap();
    }

    let json_ft = new_field_type(TypeJSON);
    let mut arr_out = new_chunk_with_capacity(std::slice::from_ref(&json_ft), 1);
    arr_f
        .append_final_result2_chunk(ec, &mut arr_pr, &mut arr_out)
        .unwrap();
    let expected_arr = parse_binary_json_from_string(
        br#"["base64:type253:QUFBQQ==", "base64:type253:QkJCQg==", "base64:type253:Q0NDQw==", "base64:type253:RERERA==", "base64:type253:WFhYWA==", "base64:type253:WVlZWQ=="]"#,
    )
    .unwrap();
    assert_eq!(
        arr_out.get_row(0).get_json(0).string(),
        expected_arr.string(),
        "JSON_ARRAYAGG must preserve earlier reused-chunk Opaque bytes (R24)"
    );

    let mut obj_out = new_chunk_with_capacity(std::slice::from_ref(&json_ft), 1);
    obj_f
        .append_final_result2_chunk(ec, &mut obj_pr, &mut obj_out)
        .unwrap();
    let expected_obj = parse_binary_json_from_string(
        br#"{"k0": "base64:type253:QUFBQQ==", "k1": "base64:type253:QkJCQg==", "k2": "base64:type253:Q0NDQw==", "k3": "base64:type253:RERERA==", "k4": "base64:type253:WFhYWA==", "k5": "base64:type253:WVlZWQ=="}"#,
    )
    .unwrap();
    assert_eq!(
        obj_out.get_row(0).get_json(0).string(),
        expected_obj.string(),
        "JSON_OBJECTAGG must preserve earlier reused-chunk Opaque bytes (R24)"
    );
}
