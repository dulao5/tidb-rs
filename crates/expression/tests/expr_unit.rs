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
        ret_type: ft,
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
        ret_type: new_field_type(tidb_base::mysql::TypeLonglong),
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
