//! Unit tests for what the govec `expr` domain cannot reach: the unexported `handle*Error`
//! helpers of `errors.go` (exempt in govec until the built-in functions of D08.2+ exercise
//! them) and the correlated-column slots (spec §3.2).

use std::sync::Arc;

use tidb_base::contextutil::{WarnHandler as _, new_static_warn_handler};
use tidb_expression::*;
use tidb_sessionctx::exprstatic as es;
use tidb_types::context::{Flags, StrictFlags};
use tidb_types::datum::new_int_datum;
use tidb_types::errors::{ErrTruncatedWrongVal, ErrWrongValue};

fn ctx(flags: Flags, levels: Option<[tidb_sessionctx::errctx::Level; 7]>) -> Arc<es::EvalContext> {
    let mut opts = vec![
        es::with_warn_handler(Some(Arc::new(new_static_warn_handler(0)))),
        es::with_type_flags(flags),
    ];
    if let Some(l) = levels {
        opts.push(es::with_err_level_map(l));
    }
    es::new_eval_context(opts)
}

#[test]
fn division_by_zero_follows_err_level() {
    use tidb_sessionctx::errctx::{LevelError, LevelWarn};
    // Default level map: division by zero is an error.
    let c = ctx(StrictFlags, None);
    let err = handle_division_by_zero_error(&*c).unwrap_err();
    assert!(ErrDivisionByZero.equal(&err));
    // ErrGroupDividedByZero at LevelWarn: a warning instead.
    let mut lm = [LevelError; 7];
    for l in lm.iter_mut() {
        *l = LevelWarn;
    }
    let c = ctx(StrictFlags, Some(lm));
    handle_division_by_zero_error(&*c).unwrap();
    assert_eq!(c.warning_count(), 1);
}

#[test]
fn allowed_packet_overflowed() {
    let c = ctx(StrictFlags, None);
    let err = handle_allowed_packet_overflowed(&*c, "repeat", 1024).unwrap_err();
    assert_eq!(
        err.to_string(),
        "[expression:1301]Result of repeat() was larger than max_allowed_packet (1024) - truncated"
    );
    let c = ctx(StrictFlags.with_truncate_as_warning(true), None);
    handle_allowed_packet_overflowed(&*c, "repeat", 1024).unwrap();
    assert_eq!(c.warning_count(), 1);
}

#[test]
fn invalid_time_error_passes_other_errors_through() {
    let c = ctx(StrictFlags, None);
    let other = ErrDivisionByZero.clone();
    assert!(ErrDivisionByZero.equal(&handle_invalid_time_error(&*c, other).unwrap_err()));
    // A time error goes through errctx (an error at the default level).
    let e = ErrWrongValue.fast_gen_by_args(vec!["DATETIME".into(), "x".into()]);
    assert!(handle_invalid_time_error(&*c, e).is_err());
    let _ = &*ErrTruncatedWrongVal;
}

#[test]
fn correlated_slot_is_shared_by_clone_and_remap() {
    let cc = CorrelatedColumn::new(Column {
        unique_id: 7,
        ..Column::default()
    });
    let mut st = EvalState::new();
    st.set_correlated(&cc, new_int_datum(42));
    let cl = cc.clone();
    assert_eq!(st.correlated(&cl).get_int64(), 42);
    let e = Expr::CorrelatedColumn(cc).into_ref();
    let m = std::collections::HashMap::from([(
        7,
        Arc::new(Column {
            unique_id: 9,
            ..Column::default()
        }),
    )]);
    let r = e.remap_column(&m).unwrap();
    assert_eq!(
        st.correlated(r.as_correlated_column().unwrap()).get_int64(),
        42
    );
    // A correlated column of the same outer column shares the slot (ExtractCorColumnsBySchema).
    let same = CorrelatedColumn::new(Column {
        unique_id: 7,
        ..Column::default()
    });
    assert_eq!(st.correlated(&same).get_int64(), 42);
    // Another outer column has its own slot: the zero (NULL) Datum until Apply writes it.
    let fresh = CorrelatedColumn::new(Column {
        unique_id: 8,
        ..Column::default()
    });
    assert!(st.correlated(&fresh).is_null());
}

/// One result type per function node (D08.1 spec §3.2, Lucky review #94): clearing
/// `ParseToJSONFlag` on `CAST(c AS JSON)` changes how the signature evaluates, as in Go
/// (v8.5.8: `CAST('[1]' AS JSON) = '[1]'` is 0 because the string is then not parsed).
/// Go shares `RetType` between clones (`Column.Clone`, `Schema.Clone`, `ScalarFunction.Clone`):
/// the clones hold the same `Arc<FieldType>`; a change copies first, so the other holders keep
/// the old type (`M-FT` copy on write, N13 §8).
#[test]
fn clones_share_ret_type_and_writes_copy() {
    use tidb_expression::builtin_cast::build_cast_function;
    use tidb_sessionctx::exprstatic as es;
    use tidb_types::field_type::new_field_type;

    let col = tidb_expression::column::Column {
        unique_id: 1,
        ret_type: Arc::new(new_field_type(tidb_base::mysql::TypeLonglong)),
        ..Default::default()
    };
    let mut c2 = col.clone();
    assert!(Arc::ptr_eq(&col.ret_type, &c2.ret_type));
    let schema = new_schema(vec![Arc::new(col.clone())]);
    let cloned = schema.clone_schema();
    assert!(Arc::ptr_eq(&cloned.columns[0].ret_type, &col.ret_type));
    c2.ret_type_mut().add_flag(tidb_base::mysql::NotNullFlag);
    assert!(!Arc::ptr_eq(&col.ret_type, &c2.ret_type));
    assert!(!tidb_base::mysql::has_not_null_flag(
        col.ret_type.get_flag()
    ));
    assert!(tidb_base::mysql::has_not_null_flag(c2.ret_type.get_flag()));

    let ev = es::new_eval_context(vec![]);
    let ctx = es::new_expr_context(vec![es::with_eval_ctx(Arc::clone(&ev))]);
    let arg = tidb_expression::expression::Expr::Column(col).into_ref();
    let cast =
        build_cast_function(&*ctx, arg, new_field_type(tidb_base::mysql::TypeVarchar)).unwrap();
    let sf = cast.as_scalar_function().unwrap();
    let mut sf2 = sf.clone();
    assert!(std::ptr::eq(sf.ret_type(), sf2.ret_type()));
    sf2.ret_type_mut().set_flen(7);
    assert!(!std::ptr::eq(sf.ret_type(), sf2.ret_type()));
    assert_ne!(sf.ret_type().get_flen(), 7);
}

#[test]
fn disable_parse_json_flag_reaches_the_signature() {
    use tidb_chunk::chunk::new_chunk_with_capacity;
    use tidb_expression::builtin_cast::build_cast_function;
    use tidb_expression::util::disable_parse_json_flag4_expr;
    use tidb_sessionctx::exprstatic as es;
    use tidb_types::datum::new_string_datum;
    use tidb_types::field_type::new_field_type;

    let ev = es::new_eval_context(vec![]);
    let ctx = es::new_expr_context(vec![es::with_eval_ctx(std::sync::Arc::clone(&ev))]);
    let ft = new_field_type(tidb_base::mysql::TypeVarchar);
    let mut chk = new_chunk_with_capacity(std::slice::from_ref(&ft), 1);
    chk.append_datum(0, &new_string_datum("[1]"));
    let col = tidb_expression::column::Column {
        unique_id: 1,
        index: 0,
        ret_type: Arc::new(ft),
        orig_name: "c".into(),
        ..Default::default()
    };
    let col = tidb_expression::expression::Expr::Column(col).into_ref();
    let mut cast =
        build_cast_function(&*ctx, col, new_field_type(tidb_base::mysql::TypeJSON)).unwrap();
    let st = tidb_expression::expression::EvalState::new();
    let parsed = cast.eval_json(&*ev, &st, chk.get_row(0)).unwrap().unwrap();
    assert_eq!(parsed.to_string(), "[1]", "the string is parsed as JSON");

    disable_parse_json_flag4_expr(&*ev, &mut cast);
    let sf = cast.as_scalar_function().unwrap();
    assert_eq!(
        sf.ret_type().get_flag() & tidb_base::mysql::ParseToJSONFlag,
        0,
        "RetType changed"
    );
    let kept = cast.eval_json(&*ev, &st, chk.get_row(0)).unwrap().unwrap();
    assert_eq!(
        kept.to_string(),
        "\"[1]\"",
        "the signature no longer parses the string"
    );
}

/// `typeInferForNull` changes a NULL constant's type to the other operand's, but not a
/// parameter marker bound to NULL: Go writes through `GetType(ctx)`, which for a parameter
/// marker is a new object each time, so the change is lost (review #95).
#[test]
fn type_infer_for_null_skips_parameter_markers() {
    use tidb_expression::constant::{ParamMarker, new_null_with_field_type};
    use tidb_expression::expression::Expr;
    use tidb_expression::scalar_function::type_infer_for_null;
    use tidb_types::field_type::new_field_type;

    let ev = es::new_eval_context(vec![es::with_param_list(&[
        tidb_types::datum::Datum::default(),
    ])]);
    let int_col = Expr::Column(tidb_expression::column::Column {
        unique_id: 1,
        index: 0,
        ret_type: Arc::new(new_field_type(tidb_base::mysql::TypeLonglong)),
        ..Default::default()
    })
    .into_ref();
    // `isNull` in typeInferForNull: a constant of type NULL with a NULL value.
    let new_null = || new_null_with_field_type(new_field_type(tidb_base::mysql::TypeNull));
    let mut marker = new_null();
    marker.param_marker = Some(ParamMarker { order: 0 });
    let mut args = vec![
        Expr::Constant(marker).into_ref(),
        Expr::Constant(new_null()).into_ref(),
        int_col,
    ];
    type_infer_for_null(&*ev, &mut args);
    let tp = |e: &Expr| match e {
        Expr::Constant(c) => c.ret_type.get_type(),
        _ => unreachable!(),
    };
    assert_eq!(
        tp(&args[0]),
        tidb_base::mysql::TypeNull,
        "parameter marker unchanged"
    );
    assert_eq!(
        tp(&args[1]),
        tidb_base::mysql::TypeLonglong,
        "NULL constant inferred"
    );
}

/// `Vectorizable`, `checkSequenceFunction`, `HasGetSetVarFunc` and `HasAssignSetVarFunc`
/// look at function names only; the functions they name (sequences, user variables) are not
/// ported yet, so the nodes here are CAST nodes renamed.
#[test]
fn vectorizable_by_function_names() {
    use tidb_expression::builtin_cast::build_cast_function;
    use tidb_expression::chunk_executor::{
        has_assign_set_var_func, has_get_set_var_func, vectorizable,
    };
    use tidb_expression::expression::{Expr, ExprRef};
    use tidb_parser::model::CIStr;
    use tidb_types::field_type::new_field_type;

    let ev = es::new_eval_context(vec![]);
    let ctx = es::new_expr_context(vec![es::with_eval_ctx(std::sync::Arc::clone(&ev))]);
    let col = Expr::Column(tidb_expression::column::Column {
        unique_id: 1,
        ret_type: Arc::new(new_field_type(tidb_base::mysql::TypeLonglong)),
        ..Default::default()
    })
    .into_ref();
    let named = |name: &str, arg: ExprRef| -> ExprRef {
        let e = build_cast_function(&*ctx, arg, new_field_type(tidb_base::mysql::TypeLonglong))
            .unwrap();
        let mut sf = e.as_scalar_function().unwrap().clone();
        sf.func_name = CIStr::new(name);
        Expr::ScalarFunction(sf).into_ref()
    };
    let plain = named("cast", Arc::clone(&col));
    assert!(vectorizable(std::slice::from_ref(&plain)));
    // GetVar / SetVar anywhere in the tree.
    let getvar = named("getvar", Arc::clone(&col));
    assert!(has_get_set_var_func(&named("cast", Arc::clone(&getvar))));
    assert!(!vectorizable(&[Arc::clone(&plain), named("cast", getvar)]));
    assert!(has_get_set_var_func(&named("setvar", Arc::clone(&col))));
    // SetVar assigns only when an argument is a function.
    assert!(!has_assign_set_var_func(&named("setvar", Arc::clone(&col))));
    assert!(has_assign_set_var_func(&named(
        "cast",
        named("setvar", Arc::clone(&plain))
    )));
    // Sequences: nextval with another sequence function, or two nextval.
    let (next, last, set) = (
        named("nextval", Arc::clone(&col)),
        named("lastval", Arc::clone(&col)),
        named("setval", Arc::clone(&col)),
    );
    assert!(vectorizable(&[Arc::clone(&next), Arc::clone(&plain)]));
    assert!(!vectorizable(&[Arc::clone(&next), Arc::clone(&last)]));
    assert!(!vectorizable(&[Arc::clone(&next), Arc::clone(&set)]));
    assert!(!vectorizable(&[Arc::clone(&next), Arc::clone(&next)]));
    assert!(vectorizable(&[last, set]));
}

/// `StringifyExpressionsWithCtx`: the expressions' strings between brackets, separated by
/// one space (Go test TestExpressionString-style; no govec op prints expressions).
#[test]
fn stringify_expressions_with_ctx() {
    use tidb_expression::expression::stringify_expressions_with_ctx;
    use tidb_types::field_type::new_field_type;
    let c = ctx(StrictFlags, None);
    let one = Expr::Constant(Constant::new(
        new_int_datum(1),
        new_field_type(tidb_base::mysql::TypeLonglong),
    ))
    .into_ref();
    let col = Expr::Column(Column {
        unique_id: 7,
        ..Column::default()
    })
    .into_ref();
    assert_eq!(stringify_expressions_with_ctx(&*c, &[]), b"[]");
    assert_eq!(
        stringify_expressions_with_ctx(&*c, &[Arc::clone(&one), Arc::clone(&col)]),
        [
            b"[".to_vec(),
            one.string_with_ctx(&*c, tidb_base::errors::RedactLogDisable),
            b" ".to_vec(),
            col.string_with_ctx(&*c, tidb_base::errors::RedactLogDisable),
            b"]".to_vec()
        ]
        .concat()
    );
}

/// `inCastContext.String` (the key is never set or read in v8.5.8).
#[test]
fn in_cast_context_string() {
    use tidb_expression::builtin_cast::inUnionCastContext;
    assert_eq!(inUnionCastContext.to_string(), "__cast_ctx");
}

/// `CNFExprs.Clone` / `Shallow`: a new list over the same (immutable, shared) nodes.
#[test]
fn cnf_exprs_clone_and_shallow() {
    let a = Expr::Constant(new_one()).into_ref();
    let b = Expr::Column(Column {
        unique_id: 3,
        ..Column::default()
    })
    .into_ref();
    let cnf: CNFExprs = vec![a, b];
    for c in [cnf_exprs_clone(&cnf), cnf_exprs_shallow(&cnf)] {
        assert_eq!(c.len(), 2);
        assert!(Arc::ptr_eq(&c[0], &cnf[0]) && Arc::ptr_eq(&c[1], &cnf[1]));
    }
    assert!(cnf_exprs_clone(&Vec::new()).is_empty());
}

/// `Schema.MemoryUsage`. The expected values are Go v8.5.8's for the same schema (columns
/// `UniqueID` 1 BIGINT `t.a` and 2 VARCHAR `t.bb`; Keys `[[c1]]`, UniqueKeys `[[c1, c2]]`).
#[test]
fn schema_memory_usage_matches_go() {
    use tidb_base::mysql::{TypeLonglong, TypeVarString};
    use tidb_types::field_type::new_field_type;
    let c1 = Arc::new(Column {
        unique_id: 1,
        ret_type: Arc::new(new_field_type(TypeLonglong)),
        orig_name: "t.a".into(),
        ..Column::default()
    });
    let c2 = Arc::new(Column {
        unique_id: 2,
        ret_type: Arc::new(new_field_type(TypeVarString)),
        orig_name: "t.bb".into(),
        ..Column::default()
    });
    assert_eq!((c1.memory_usage(), c2.memory_usage()), (303, 310));
    assert_eq!(new_schema(Vec::new()).memory_usage(), 72);
    let mut s = new_schema(vec![Arc::clone(&c1), Arc::clone(&c2)]);
    assert_eq!(s.memory_usage(), 701);
    s.keys = vec![vec![Arc::clone(&c1)]];
    s.unique_keys = vec![vec![c1, c2]];
    assert_eq!(s.memory_usage(), 1689);
}

/// `Column.EvalVirtualColumn` evaluates the column's `VirtualExpr` on the row.
#[test]
fn eval_virtual_column_evaluates_the_expression() {
    use tidb_chunk::chunk::new_chunk_with_capacity;
    let c = Column {
        unique_id: 5,
        virtual_expr: Some(Expr::Constant(new_one()).into_ref()),
        ..Column::default()
    };
    let chk = new_chunk_with_capacity::<tidb_types::etc::FieldType>(&[], 1);
    let ctx = ctx(Flags::default(), None);
    let st = EvalState::new();
    let d = c.eval_virtual_column(&*ctx, &st, chk.get_row(0)).unwrap();
    assert_eq!(d.get_int64(), 1);
}
