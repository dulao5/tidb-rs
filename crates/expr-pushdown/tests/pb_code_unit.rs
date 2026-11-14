//! Unit tests of the pb codes the push-down code reads (spec `specs/D08.3-pushdown` §5).

use protobuf::ProtobufEnum;
use tidb_base::mysql;
use tidb_expr_pushdown::tipb_sig::{self, scalar_func_sig_string};
use tidb_expression::ast_funcs as ast;
use tidb_expression::scalar_function::new_function;
use tidb_expression::{Column, Expr, ExprRef};
use tidb_sessionctx::exprstatic as es;
use tidb_types::field_type::new_field_type;

/// Every value of Go's `tipb.ScalarFuncSig` (the revision of TiDB v8.5.8) is a value of the
/// Rust `tipb` enum, so `pb_expr` can always put a signature's pb code into a `tipb::Expr`.
#[test]
fn pb_codes() {
    let mut n = 0;
    for code in 0..10_000 {
        let name = scalar_func_sig_string(code);
        if name == code.to_string() {
            continue;
        }
        n += 1;
        assert!(
            tipb::ScalarFuncSig::from_i32(code).is_some(),
            "{name} = {code} is not a value of the Rust tipb ScalarFuncSig"
        );
    }
    assert_eq!(n, 631);
    assert_eq!(scalar_func_sig_string(tipb_sig::LTJson), "LTJson");
    assert_eq!(scalar_func_sig_string(-1), "-1");
    assert_eq!(scalar_func_sig_string(4574), "4574"); // UUIDv4: newer tipb only
}

fn col(tp: u8) -> ExprRef {
    Expr::Column(Column {
        unique_id: 1,
        ret_type: new_field_type(tp),
        ..Column::default()
    })
    .into_ref()
}

/// P1-1 (D08.1, Lucky's review of #102): the IS TRUE / IS FALSE signatures with `keepNull`
/// get their pb code as the plain signature's `+ 20` (`builtin_op.rs`); check the six codes
/// against tipb.
#[test]
fn is_true_or_false_with_null_pb_codes() {
    let ctx = es::new_expr_context(vec![]);
    let cases = [
        (
            ast::IsTruthWithNull,
            mysql::TypeLonglong,
            tipb_sig::IntIsTrueWithNull,
        ),
        (
            ast::IsTruthWithNull,
            mysql::TypeDouble,
            tipb_sig::RealIsTrueWithNull,
        ),
        (
            ast::IsTruthWithNull,
            mysql::TypeNewDecimal,
            tipb_sig::DecimalIsTrueWithNull,
        ),
        (ast::IsFalsity, mysql::TypeLonglong, tipb_sig::IntIsFalse),
        (
            ast::IsTruthWithoutNull,
            mysql::TypeLonglong,
            tipb_sig::IntIsTrue,
        ),
    ];
    for (name, tp, want) in cases {
        let e = new_function(
            &*ctx,
            name,
            new_field_type(mysql::TypeLonglong),
            vec![col(tp)],
        )
        .unwrap();
        let Expr::ScalarFunction(sf) = &*e else {
            panic!("{name}: not a scalar function")
        };
        assert_eq!(
            sf.function.pb_code(),
            want,
            "{name}({tp}): {}",
            scalar_func_sig_string(sf.function.pb_code())
        );
    }
    // The `+ 20` of `builtin_op.rs` for all six signatures (IS FALSE with keepNull has no
    // function class in v8.5.8: only PBToExpr builds it).
    use tidb_expression::builtin_op as op;
    for (plain, want) in [
        (&op::builtinIntIsTrueSig, tipb_sig::IntIsTrueWithNull),
        (&op::builtinRealIsTrueSig, tipb_sig::RealIsTrueWithNull),
        (
            &op::builtinDecimalIsTrueSig,
            tipb_sig::DecimalIsTrueWithNull,
        ),
        (&op::builtinIntIsFalseSig, tipb_sig::IntIsFalseWithNull),
        (&op::builtinRealIsFalseSig, tipb_sig::RealIsFalseWithNull),
        (
            &op::builtinDecimalIsFalseSig,
            tipb_sig::DecimalIsFalseWithNull,
        ),
    ] {
        assert_eq!(plain.pb_code + 20, want, "{}", plain.name);
    }
}
