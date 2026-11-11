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
