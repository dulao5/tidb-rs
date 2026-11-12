//! The two drivers `#[eval_fn]` generates agree row by row (values, NULLs, errors), for a
//! strict and a lazy body, fixed- and variable-length results, NULL inputs and a NULL result.

use std::sync::Arc;

use tidb_base::{Error, mysql};
use tidb_chunk::chunk::new_chunk_with_capacity;
use tidb_chunk::column::new_empty_column;
use tidb_expression::eval::{Args, BuiltinFunc, RpnProgram, RpnState, SigDesc, SigView};
use tidb_expression::{Column, EvalContext, EvalState, Expr, ExprRef, ScalarFunction, eval_fn};
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
    args_fallback: tidb_expression::eval::ArgsFallback::None,
};
static CAT_SIG: SigDesc = SigDesc {
    name: "cat",
    pb_code: 0,
    vectorized: true,
    drivers: CAT,
    args_fallback: tidb_expression::eval::ArgsFallback::None,
};

fn col(i: isize, ft: &FieldType) -> ExprRef {
    Arc::new(Expr::Column(Column {
        index: i,
        ret_type: ft.clone(),
        ..Column::default()
    }))
}

fn func(sig: &'static SigDesc, args: Vec<ExprRef>, ret: FieldType) -> ExprRef {
    let function = BuiltinFunc::new(sig, args, ret.clone(), "utf8mb4", "utf8mb4_bin");
    Arc::new(Expr::ScalarFunction(ScalarFunction::new(
        sig.name, ret, function,
    )))
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

fn no_flags(_f: &impl SigView) -> u8 {
    0
}

#[eval_fn(Int, checked(nflags = 0, flags = no_flags, err = add_err))]
fn checked_add<const FL: u8>(a: i64, b: i64) -> (i64, bool) {
    a.overflowing_add(b)
}

#[cold]
fn add_err(_ctx: &dyn EvalContext, _f: &impl SigView, a: i64, b: i64) -> Error {
    Error::plain(format!("overflow {a} + {b}"))
}

static CHECKED_ADD_SIG: SigDesc = SigDesc {
    name: "checked_add",
    pb_code: 0,
    vectorized: true,
    drivers: CHECKED_ADD,
    args_fallback: tidb_expression::eval::ArgsFallback::None,
};

/// The checked form: an overflow in a NULL row is not an error; the error is the one of
/// the first non-NULL overflowing row, in both drivers.
#[test]
fn checked_drivers_agree() {
    let int = new_field_type(mysql::TypeLonglong);
    let ctx = new_eval_context(vec![]);
    let ctx: &dyn EvalContext = &*ctx;
    let st = EvalState::new();
    let e = func(
        &CHECKED_ADD_SIG,
        vec![col(0, &int), col(1, &int)],
        int.clone(),
    );
    let prog = RpnProgram::compile(&e);
    let mut rpn = RpnState::default();
    for n in [1usize, 7, 8, 9, 17, 64, 100] {
        for bad_rows in [vec![], vec![n - 1], vec![0, n / 2], vec![n / 3, n - 1]] {
            for null_rows in [
                vec![],
                vec![0],
                bad_rows.first().copied().into_iter().collect::<Vec<_>>(),
            ] {
                let mut chk = new_chunk_with_capacity(&[int.clone(), int.clone()], n);
                for i in 0..n {
                    // A NULL row keeps an overflowing value under its NULL bit (as an
                    // intermediate column of the vector driver can).
                    if bad_rows.contains(&i) {
                        chk.append_int64(0, i64::MAX - i as i64);
                    } else {
                        chk.append_int64(0, i as i64);
                    }
                    chk.append_int64(1, 1000);
                }
                for &i in &null_rows {
                    chk.column_mut(0).set_null(i, true);
                }
                let mut res = new_empty_column(&int);
                let vec_r = prog.eval(ctx, &st, &mut rpn, &chk, &mut res);
                let mut row_err = None;
                for i in 0..n {
                    match e.eval_int(ctx, &st, chk.get_row(i)) {
                        Err(err) => {
                            row_err = Some(err.to_string());
                            break;
                        }
                        Ok(v) => {
                            if vec_r.is_ok() {
                                assert_eq!(v.is_none(), res.is_null(i), "n {n} row {i}");
                                if let Some(v) = v {
                                    assert_eq!(res.get_int64(i), v, "n {n} row {i}");
                                }
                            }
                        }
                    }
                }
                assert_eq!(
                    vec_r.err().map(|e| e.to_string()),
                    row_err,
                    "n {n} bad {bad_rows:?} null {null_rows:?}"
                );
            }
        }
    }
}

/// A FLOAT, ENUM or DOUBLE column fed to a body that reads int64 values: Go's
/// `Column.VecEvalInt` would copy storage that does not hold int64s, so the RPN refuses to
/// compile it instead of letting the signature misread the bytes (Lucky #82).
#[test]
fn rpn_refuses_misread_columns() {
    let int = new_field_type(mysql::TypeLonglong);
    for tp in [mysql::TypeFloat, mysql::TypeDouble, mysql::TypeNewDecimal] {
        let ft = new_field_type(tp);
        let e = func(&ADD_SIG, vec![col(0, &ft), col(0, &ft)], int.clone());
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| RpnProgram::compile(&e)));
        let msg = r.expect_err("compile must refuse");
        let msg = msg.downcast_ref::<String>().cloned().unwrap_or_default();
        assert!(msg.contains("read as"), "{tp}: {msg}");
    }
    // A BIT column read as int converts (hybrid), it is accepted.
    let mut bit = new_field_type(mysql::TypeBit);
    bit.set_flen(8);
    let e = func(&ADD_SIG, vec![col(0, &bit), col(0, &bit)], int.clone());
    let _ = RpnProgram::compile(&e);
}

// A variadic lazy body whose vector version reads the first argument as INT and the rest as
// STRING (`args(..)`, D08.1 spec §4.1).
#[eval_fn(String, lazy, args(Int, String))]
fn int_then_strs(
    _ctx: &dyn EvalContext,
    _f: &impl SigView,
    _args: &mut impl Args,
    _out: &mut Vec<u8>,
) -> Result<Option<()>, Error> {
    Ok(Some(()))
}

#[test]
fn arg_eval_types_of_lazy_bodies() {
    use tidb_parser::types::{ETInt, ETString};
    // Without args(..): every argument is read as its own type.
    assert_eq!(CAT.arg_eval_type(0), None);
    assert_eq!(CAT.arg_eval_type(3), None);
    // args(Int, String): the last type repeats for the extra arguments.
    assert_eq!(INT_THEN_STRS.arg_eval_type(0), Some(ETInt));
    assert_eq!(INT_THEN_STRS.arg_eval_type(1), Some(ETString));
    assert_eq!(INT_THEN_STRS.arg_eval_type(5), Some(ETString));
    // A strict body: its parameters.
    assert_eq!(ADD_OR_NULL.arg_eval_type(1), Some(ETInt));
}
