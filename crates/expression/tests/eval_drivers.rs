//! The two drivers `#[eval_fn]` generates agree row by row (values, NULLs, errors), for a
//! strict and a lazy body, fixed- and variable-length results, NULL inputs and a NULL result.

use std::sync::Arc;

use tidb_base::{Error, mysql};
use tidb_chunk::chunk::new_chunk_with_capacity;
use tidb_chunk::column::new_empty_column;
use tidb_expression::eval::{Args, BuiltinFunc, RpnProgram, RpnState, SigDesc, SigView};
use tidb_expression::{Column, EvalContext, EvalState, Expr, ExprRef, ScalarFunction, eval_fn};
use tidb_parser::model::CIStr;
use tidb_sessionctx::exprstatic::new_eval_context;
use tidb_types::etc::FieldType;
use tidb_types::field_type::new_field_type;

// a + b; NULL when a == 7; an error when a == 13.
#[eval_fn(Int)]
fn add_or_null(
    _ctx: &dyn EvalContext,
    _f: &impl SigView,
    a: i64,
    b: i64,
) -> Result<Option<i64>, Error> {
    if a == 13 {
        return Err(Error::plain("thirteen"));
    }
    if a == 7 {
        return Ok(None);
    }
    Ok(Some(a.wrapping_add(b)))
}

// CONCAT of all arguments, NULL at the first NULL argument.
#[eval_fn(String, lazy)]
fn cat(
    ctx: &dyn EvalContext,
    _f: &impl SigView,
    args: &mut impl Args,
    out: &mut Vec<u8>,
) -> Result<Option<()>, Error> {
    for i in 0..args.len() {
        let Some(d) = args.string(ctx, i)? else {
            return Ok(None);
        };
        out.extend_from_slice(&d);
    }
    Ok(Some(()))
}

static ADD_SIG: SigDesc = SigDesc {
    name: "add",
    pb_code: 0,
    vectorized: true,
    drivers: ADD_OR_NULL,
};
static CAT_SIG: SigDesc = SigDesc {
    name: "cat",
    pb_code: 0,
    vectorized: true,
    drivers: CAT,
};

fn col(i: isize, ft: &FieldType) -> ExprRef {
    Arc::new(Expr::Column(Column {
        index: i,
        ret_type: ft.clone(),
        ..Column::default()
    }))
}

fn func(sig: &'static SigDesc, args: Vec<ExprRef>, ret: FieldType) -> ExprRef {
    let function = BuiltinFunc::new(sig, args, ret.clone(), "utf8mb4_bin".into());
    Arc::new(Expr::ScalarFunction(ScalarFunction {
        func_name: CIStr::new(sig.name),
        ret_type: ret,
        function,
    }))
}

#[test]
fn int_drivers_agree() {
    let int = new_field_type(mysql::TypeLonglong);
    let mut chk = new_chunk_with_capacity(&[int.clone(), int.clone()], 100);
    for i in 0..100i64 {
        if i % 5 == 0 {
            chk.append_null(0)
        } else {
            chk.append_int64(0, i % 12)
        }
        if i % 7 == 0 {
            chk.append_null(1)
        } else {
            chk.append_int64(1, i)
        }
    }
    // add(add(c0, c1), c1): a nested call, so an intermediate column is used.
    let inner = func(&ADD_SIG, vec![col(0, &int), col(1, &int)], int.clone());
    let e = func(&ADD_SIG, vec![inner, col(1, &int)], int.clone());
    let ctx = new_eval_context(vec![]);
    let ctx: &dyn EvalContext = &*ctx;
    let st = EvalState::new();
    let prog = RpnProgram::compile(&e);
    let mut rpn = RpnState::default();
    let mut res = new_empty_column(&int);
    prog.eval(ctx, &st, &mut rpn, &chk, &mut res).unwrap();
    let mut nulls = 0;
    for i in 0..100 {
        let row = e.eval_int(ctx, &st, chk.get_row(i)).unwrap();
        match row {
            None => {
                nulls += 1;
                assert!(res.is_null(i), "row {i}");
            }
            Some(v) => {
                assert!(!res.is_null(i), "row {i}");
                assert_eq!(res.get_int64(i), v, "row {i}");
            }
        }
    }
    assert!(nulls > 20 && nulls < 80);
    // Run twice: the pooled intermediate column is reused.
    let mut res2 = new_empty_column(&int);
    prog.eval(ctx, &st, &mut rpn, &chk, &mut res2).unwrap();
    for i in 0..100 {
        assert_eq!(res.is_null(i), res2.is_null(i));
        if !res.is_null(i) {
            assert_eq!(res.get_int64(i), res2.get_int64(i));
        }
    }
    // An error in a row that is not NULL stops both drivers.
    let mut chk = new_chunk_with_capacity(&[int.clone(), int.clone()], 3);
    for (a, b) in [(1, 1), (13, 1), (2, 2)] {
        chk.append_int64(0, a);
        chk.append_int64(1, b);
    }
    let e = func(&ADD_SIG, vec![col(0, &int), col(1, &int)], int.clone());
    let prog = RpnProgram::compile(&e);
    assert!(prog.eval(ctx, &st, &mut rpn, &chk, &mut res).is_err());
    assert!(e.eval_int(ctx, &st, chk.get_row(1)).is_err());
}

#[test]
fn lazy_string_drivers_agree() {
    let s = new_field_type(mysql::TypeVarString);
    let mut chk = new_chunk_with_capacity(&[s.clone(), s.clone()], 50);
    for i in 0..50 {
        if i % 4 == 0 {
            chk.append_null(0)
        } else {
            chk.append_string(0, format!("a{i}").as_bytes())
        }
        if i % 6 == 0 {
            chk.append_null(1)
        } else {
            chk.append_string(1, format!("-{i}").as_bytes())
        }
    }
    let e = func(&CAT_SIG, vec![col(0, &s), col(1, &s)], s.clone());
    let ctx = new_eval_context(vec![]);
    let ctx: &dyn EvalContext = &*ctx;
    let st = EvalState::new();
    let prog = RpnProgram::compile(&e);
    let mut rpn = RpnState::default();
    let mut res = new_empty_column(&s);
    prog.eval(ctx, &st, &mut rpn, &chk, &mut res).unwrap();
    for i in 0..50 {
        match e.eval_string(ctx, &st, chk.get_row(i)).unwrap() {
            None => assert!(res.is_null(i), "row {i}"),
            Some(v) => assert_eq!(res.get_bytes(i), &v[..], "row {i}"),
        }
    }
}
