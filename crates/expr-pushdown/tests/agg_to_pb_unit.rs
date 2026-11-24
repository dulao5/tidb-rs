//! Go unit test of `pkg/expression/aggregation/agg_to_pb_test.go` (`TestAggFunc2Pb`, D08.11
//! spec §6.1): aggregate function descriptors to `tipb.Expr`.
//!
//! Go compares `json.Marshal` of the message with a literal; the replay compares the same
//! fields of the message.

use std::sync::Arc;

use tidb_base::kv::UnSpecified;
use tidb_base::mysql::{TypeDouble, TypeLonglong, TypeVarchar};
use tidb_expr_pushdown::aggregation::{
    agg_func_to_pb_expr, agg_function_mode_to_pb, pb_agg_func_mode_to_agg_func_mode,
    pb_expr_to_agg_func_desc,
};
use tidb_expr_pushdown::new_push_down_context;
use tidb_expression::aggregation::{AggFunctionMode, new_agg_func_desc};
use tidb_expression::ast_funcs as ast;
use tidb_expression::{BuildContext, Column, Expr, ExprRef};
use tidb_kv::checker::RequestTypeSupportedChecker;
use tidb_kv::kv::{Client, ClientSendOption, Request, Response};
use tidb_sessionctx::exprstatic as es;
use tidb_types::collate::set_new_collation_enabled_for_test;
use tidb_types::field_type::new_field_type;

/// mock.Client: kv.RequestTypeSupportedChecker.
struct MockClient;

impl Client for MockClient {
    fn send(
        &self,
        _: &tidb_kv::Context,
        _: &Request,
        _: Option<tidb_kv::kv::OptionValue>,
        _: &ClientSendOption,
    ) -> Box<dyn Response> {
        unreachable!("mock.Client.Send")
    }

    fn is_request_type_supported(&self, req_type: i64, sub_type: i64) -> bool {
        RequestTypeSupportedChecker.is_request_type_supported(req_type, sub_type)
    }
}

// genColumn (agg_to_pb_test.go).
fn gen_column(tp: u8, id: i64) -> ExprRef {
    Expr::Column(Column {
        ret_type: Arc::new(new_field_type(tp)),
        id,
        index: id as isize,
        ..Column::default()
    })
    .into_ref()
}

/// The fields of Go's JSON of a `tipb.FieldType`: tp, flag, flen, decimal, collate, charset,
/// array.
type PbFt = (i32, u32, i32, i32, i32, String, bool);

fn ft(f: &tipb::FieldType) -> PbFt {
    (
        f.get_tp(),
        f.get_flag(),
        f.get_flen(),
        f.get_decimal(),
        f.get_collate(),
        f.get_charset().to_owned(),
        f.get_array(),
    )
}

fn double_ft() -> PbFt {
    (5, 0, -1, -1, -63, "binary".to_owned(), false)
}

#[test]
fn agg_func_to_pb() {
    // testsetup.SetupForCommonTest enables the new collations.
    set_new_collation_enabled_for_test(true);
    let ctx = es::new_expr_context(vec![]);
    let client: Arc<dyn Client> = Arc::new(MockClient);

    let func_names = [
        ast::AggFuncSum,
        ast::AggFuncCount,
        ast::AggFuncAvg,
        ast::AggFuncGroupConcat,
        ast::AggFuncMax,
        ast::AggFuncMin,
        ast::AggFuncFirstRow,
    ];
    let func_types = [
        new_field_type(TypeDouble),
        new_field_type(TypeLonglong),
        new_field_type(TypeDouble),
        new_field_type(TypeVarchar),
        new_field_type(TypeDouble),
        new_field_type(TypeDouble),
        new_field_type(TypeDouble),
    ];
    // The expected tp, val and field type of each JSON literal of the Go test.
    let want: [(i32, Option<Vec<u8>>, PbFt); 7] = [
        (3002, None, double_ft()),
        (3001, None, (8, 0, 20, 0, -63, "binary".to_owned(), false)),
        (3003, None, double_ft()),
        (
            3007,
            Some(vec![0, 0, 0, 0, 0, 0, 4, 0]),
            (15, 0, -1, -1, -46, "utf8mb4".to_owned(), false),
        ),
        (3005, None, double_ft()),
        (3004, None, double_ft()),
        (3006, None, double_ft()),
    ];
    for (i, func_name) in func_names.iter().enumerate() {
        for has_distinct in [true, false] {
            let args = vec![gen_column(TypeDouble, 1)];
            let mut agg_func = new_agg_func_desc(&*ctx, func_name, args, has_distinct).unwrap();
            agg_func.ret_tp = Arc::new(func_types[i].clone());
            // NewPushDownContextFromSessionVars of mock.NewContext: group_concat_max_len 1024.
            let push_ctx = new_push_down_context(
                ctx.get_eval_ctx(),
                Arc::clone(&client),
                false,
                None,
                None,
                1024,
            );
            let pb = agg_func_to_pb_expr(&push_ctx, &agg_func, UnSpecified).unwrap();

            let (tp, val, ret) = &want[i];
            assert_eq!(pb.get_tp() as i32, *tp, "{func_name}");
            assert_eq!(pb.has_val().then(|| pb.get_val().to_vec()), *val);
            assert_eq!(pb.get_children().len(), 1);
            let child = &pb.get_children()[0];
            assert_eq!(child.get_tp() as i32, 201);
            // "gAAAAAAAAAE=": codec.EncodeInt(nil, 1).
            assert_eq!(child.get_val(), &[0x80, 0, 0, 0, 0, 0, 0, 1]);
            assert_eq!(child.get_sig() as i32, 0);
            assert!(child.has_sig());
            assert_eq!(ft(child.get_field_type()), double_ft());
            assert!(!child.get_has_distinct());
            assert!(child.has_has_distinct());
            assert!(pb.has_sig());
            assert_eq!(pb.get_sig() as i32, 0);
            assert_eq!(ft(pb.get_field_type()), *ret);
            assert!(pb.has_has_distinct());
            assert_eq!(pb.get_has_distinct(), has_distinct);
            assert!(pb.has_agg_func_mode());
            assert_eq!(pb.get_agg_func_mode() as i32, 0);
            assert!(pb.get_order_by().is_empty());
        }
    }
}

// The reverse direction (no Go unit test; the govec domain `aggdesc` covers it too).
#[test]
fn pb_to_agg_func_desc_round_trip() {
    set_new_collation_enabled_for_test(true);
    let ctx = es::new_expr_context(vec![]);
    let client: Arc<dyn Client> = Arc::new(MockClient);
    let mut agg_func = new_agg_func_desc(
        &*ctx,
        ast::AggFuncSum,
        vec![gen_column(TypeDouble, 1)],
        true,
    )
    .unwrap();
    agg_func.mode = AggFunctionMode::Partial2Mode;
    let push_ctx = new_push_down_context(ctx.get_eval_ctx(), client, false, None, None, 1024);
    let pb = agg_func_to_pb_expr(&push_ctx, &agg_func, UnSpecified).unwrap();

    let field_tps = [new_field_type(TypeLonglong), new_field_type(TypeDouble)];
    let desc = pb_expr_to_agg_func_desc(&*ctx, &pb, &field_tps).unwrap();
    assert_eq!(desc.name, ast::AggFuncSum);
    assert_eq!(desc.mode, AggFunctionMode::Partial2Mode);
    assert!(!desc.has_distinct);
    assert_eq!(*desc.ret_tp, *agg_func.ret_tp);

    let mut unknown = pb.clone();
    unknown.set_tp(tipb::ExprType::StddevPop);
    let err = pb_expr_to_agg_func_desc(&*ctx, &unknown, &field_tps).unwrap_err();
    assert_eq!(
        err.to_string(),
        "unknown aggregation function type: StddevPop"
    );

    assert_eq!(
        pb_agg_func_mode_to_agg_func_mode(None),
        AggFunctionMode::Partial1Mode
    );
    for mode in [
        AggFunctionMode::CompleteMode,
        AggFunctionMode::FinalMode,
        AggFunctionMode::Partial1Mode,
        AggFunctionMode::Partial2Mode,
        AggFunctionMode::DedupMode,
    ] {
        assert_eq!(
            pb_agg_func_mode_to_agg_func_mode(Some(agg_function_mode_to_pb(mode))),
            mode
        );
    }
}
