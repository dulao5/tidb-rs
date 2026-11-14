//! go-file: pkg/expression/expr_to_pb.go
//!
//! The conversion of expressions to `tipb.Expr` (spec `specs/D08.3-pushdown` §3).
//!
//! - Go's `*tipb.Expr` result, nil when the expression cannot be converted, is
//!   `Option<tipb::Expr>`.
//! - The gogoproto messages have non-nullable fields that Go always marshals (`Expr.tp`,
//!   `Expr.sig`, `Expr.has_distinct`, every field of `FieldType`, `ByItem.desc`); the Rust
//!   `tipb` messages only marshal the fields that are set, so the struct literals of Go are
//!   `pb_expr` / [`to_pb_field_type`], which set them all: the marshalled bytes are Go's
//!   (spec §3.1).
//! - Constants and correlated columns are evaluated with an [`EvalState`] (N4 §3.4, spec
//!   §3.2): Go reads
//!   `CorrelatedColumn.Data`, which is the zero Datum while planning (the planner's
//!   conversions use [`empty_eval_state`]) and the outer row's value when an executor rebuilds
//!   its request.

// Go identifiers for constants and statics (mapping rule M-GONAME).
#![allow(non_upper_case_globals)]

use std::sync::OnceLock;

use protobuf::Message;
use protobuf::ProtobufEnum;
use protobuf::RepeatedField;
use tidb_base::Error;
use tidb_base::dbterror::plannererrors;
use tidb_base::errors::RedactLogDisable;
use tidb_base::gofmt::Arg;
use tidb_base::kv::{StoreType, TiFlash, TiKV, UnSpecified};
use tidb_base::mysql;
use tidb_chunk::Row;
use tidb_codec::codec::encode_mysql_time;
use tidb_codec::decimal::encode_decimal;
use tidb_codec::float::encode_float;
use tidb_codec::number::{encode_int, encode_uint};
use tidb_expression::context::{err_ctx, type_ctx};
use tidb_expression::{Column, EvalContext, EvalState, Expr, ExprRef, ScalarFunction};
use tidb_kv::kv::{Client, ReqSubTypeBasic, ReqTypeDAG, ReqTypeSelect};
use tidb_parser::types::type_str;
use tidb_types::collate;
use tidb_types::datum::{
    Datum, KindBinaryLiteral, KindBytes, KindFloat32, KindFloat64, KindInt64, KindMysqlBit,
    KindMysqlDecimal, KindMysqlDuration, KindMysqlEnum, KindMysqlTime, KindNull, KindString,
    KindUint64, KindVectorFloat32,
};
use tidb_types::etc::FieldType;
use tidb_types::field_type_builder::new_field_type_builder;
use tipb::{ByItem, ExprType, ScalarFuncSig};

use crate::infer_pushdown::{can_func_be_pushed, is_push_down_enabled};
use crate::tipb_sig::Unspecified;

/// The state without correlated values: Go's zero `CorrelatedColumn.Data` while planning.
// glue: the EvalState of plan-time conversions (N4 §3.4)
pub fn empty_eval_state() -> &'static EvalState {
    static EMPTY: OnceLock<EvalState> = OnceLock::new();
    EMPTY.get_or_init(EvalState::new)
}

/// A `tipb.Expr` literal: the non-nullable fields (`tp`, `sig`, `has_distinct`) are always
/// set, as gogoproto always marshals them.
// glue: Go's &tipb.Expr{...} literal (gogoproto non-nullable fields)
pub(crate) fn pb_expr(
    tp: ExprType,
    val: Option<Vec<u8>>,
    sig: i32,
    children: Vec<tipb::Expr>,
    field_type: Option<tipb::FieldType>,
) -> Option<tipb::Expr> {
    let mut e = tipb::Expr::default();
    e.set_tp(tp);
    if let Some(v) = val {
        e.set_val(v);
    }
    e.set_children(RepeatedField::from_vec(children));
    // Every pb code of TiDB v8.5.8 is a value of the (newer) Rust tipb enum (unit test
    // `pb_codes`); a code it does not know cannot be put in a tipb::Expr.
    e.set_sig(ScalarFuncSig::from_i32(sig)?);
    if let Some(ft) = field_type {
        e.set_field_type(ft);
    }
    e.set_has_distinct(false);
    Some(e)
}

/// ExpressionsToPBList converts expressions to tipb.Expr list for new plan.
// go: ExpressionsToPBList L1
pub fn expressions_to_pb_list(
    ctx: &dyn EvalContext,
    st: &EvalState,
    exprs: &[ExprRef],
    client: &dyn Client,
) -> Result<Vec<tipb::Expr>, Error> {
    let pc = PbConverter { client, ctx, st };
    let mut pb_expr = Vec::with_capacity(exprs.len());
    for expr in exprs {
        let Some(v) = pc.expr_to_pb(expr) else {
            return Err(cannot_be_pushed_down(ctx, expr));
        };
        pb_expr.push(v);
    }
    Ok(pb_expr)
}

/// `plannererrors.ErrInternal.GenWithStack("expression %v cannot be pushed down", ...)`.
// glue: the error of ExpressionsToPBList / ProjectionExpressionsToPBList
fn cannot_be_pushed_down(ctx: &dyn EvalContext, expr: &Expr) -> Error {
    plannererrors::ErrInternal.gen_with_stack(
        "expression %v cannot be pushed down",
        vec![Arg::Str(expr.string_with_ctx(ctx, RedactLogDisable).into())],
    )
}

/// ProjectionExpressionsToPBList converts PhysicalProjection's expressions to tipb.Expr list
/// for new plan. It doesn't check type for top level column expression, since top level
/// column expression doesn't imply any calculations
// go: ProjectionExpressionsToPBList L1
pub fn projection_expressions_to_pb_list(
    ctx: &dyn EvalContext,
    st: &EvalState,
    exprs: &[ExprRef],
    client: &dyn Client,
) -> Result<Vec<tipb::Expr>, Error> {
    let pc = PbConverter { client, ctx, st };
    let mut pb_expr = Vec::with_capacity(exprs.len());
    for expr in exprs {
        let v = if let Expr::Column(column) = &**expr {
            pc.column_to_pb_expr(column, false)
        } else {
            pc.expr_to_pb(expr)
        };
        let Some(v) = v else {
            return Err(cannot_be_pushed_down(ctx, expr));
        };
        pb_expr.push(v);
    }
    Ok(pb_expr)
}

/// PbConverter supplies methods to convert TiDB expressions to TiPB.
#[derive(Clone, Copy)]
pub struct PbConverter<'a> {
    client: &'a dyn Client,
    ctx: &'a dyn EvalContext,
    /// The values of correlated columns (N4 §3.4).
    st: &'a EvalState,
}

/// NewPBConverter creates a PbConverter.
// go: NewPBConverter L1
pub fn new_pb_converter<'a>(
    client: &'a dyn Client,
    ctx: &'a dyn EvalContext,
    st: &'a EvalState,
) -> PbConverter<'a> {
    PbConverter { client, ctx, st }
}

impl PbConverter<'_> {
    /// ExprToPB converts Expression to TiPB.
    // go: PbConverter.ExprToPB L1
    pub fn expr_to_pb(&self, expr: &Expr) -> Option<tipb::Expr> {
        match expr {
            Expr::Constant(_) => {
                let pb_expr = self.con_or_cor_col_to_pb_expr(expr)?;
                Some(pb_expr)
            }
            Expr::CorrelatedColumn(_) => self.con_or_cor_col_to_pb_expr(expr),
            Expr::Column(x) => self.column_to_pb_expr(x, true),
            Expr::ScalarFunction(x) => self.scalar_func_to_pb_expr(x),
        }
    }

    // go: PbConverter.conOrCorColToPBExpr L1
    pub(crate) fn con_or_cor_col_to_pb_expr(&self, expr: &Expr) -> Option<tipb::Expr> {
        let ft = expr.get_type(self.ctx);
        let d = match expr.eval(self.ctx, self.st, Row::default()) {
            Ok(d) => d,
            Err(err) => {
                log::error!(
                    "eval constant or correlated column expression={} error={}",
                    String::from_utf8_lossy(&expr.string_with_ctx(self.ctx, RedactLogDisable)),
                    err
                );
                return None;
            }
        };
        let (tp, val, ok) = self.encode_datum(&ft, &d);
        if !ok {
            return None;
        }

        if !self
            .client
            .is_request_type_supported(ReqTypeSelect, tp as i64)
        {
            return None;
        }
        pb_expr(
            tp,
            val,
            Unspecified,
            Vec::new(),
            Some(to_pb_field_type(&ft)),
        )
    }

    /// Go's `val` is nil only for NULL (and on the failure paths): `None`.
    // go: PbConverter.encodeDatum L1
    fn encode_datum(&self, ft: &FieldType, d: &Datum) -> (ExprType, Option<Vec<u8>>, bool) {
        let tp;
        let mut val = Vec::new();
        match d.kind() {
            KindNull => return (ExprType::Null, None, true),
            KindInt64 => {
                tp = ExprType::Int64;
                encode_int(&mut val, d.get_int64());
            }
            KindUint64 => {
                tp = ExprType::Uint64;
                encode_uint(&mut val, d.get_uint64());
            }
            KindString | KindBinaryLiteral => {
                tp = ExprType::String;
                val = d.get_bytes().to_vec();
            }
            KindMysqlBit => {
                tp = ExprType::MysqlBit;
                val = d.get_bytes().to_vec();
            }
            KindBytes => {
                tp = ExprType::Bytes;
                val = d.get_bytes().to_vec();
            }
            KindFloat32 => {
                tp = ExprType::Float32;
                encode_float(&mut val, d.get_float64());
            }
            KindFloat64 => {
                tp = ExprType::Float64;
                encode_float(&mut val, d.get_float64());
            }
            KindMysqlDuration => {
                tp = ExprType::MysqlDuration;
                encode_int(&mut val, d.get_mysql_duration().duration);
            }
            KindMysqlDecimal => {
                tp = ExprType::MysqlDecimal;
                // Use precision and fraction from MyDecimal instead of the ones in datum itself.
                // These two set of parameters are not the same. MyDecimal is compatible with MySQL
                // so the precision and fraction from MyDecimal are consistent with MySQL. The other
                // ones come from the column type which belongs to the output schema. Here the datum
                // are encoded into protobuf and will be used to do calculation so it should use the
                // MyDecimal precision and fraction otherwise there may be a loss of accuracy.
                if let Err(err) = encode_decimal(&mut val, d.get_mysql_decimal(), 0, 0) {
                    log::error!("encode decimal error={err}");
                    return (tp, None, false);
                }
            }
            KindMysqlTime => {
                if self
                    .client
                    .is_request_type_supported(ReqTypeDAG, ExprType::MysqlTime as i64)
                {
                    let tp = ExprType::MysqlTime;
                    let (tc, ec) = (type_ctx(self.ctx), err_ctx(self.ctx));
                    let res = encode_mysql_time(
                        &tc.location(),
                        d.get_mysql_time(),
                        ft.get_type(),
                        &mut val,
                    );
                    let res = match res {
                        Ok(()) => Ok(()),
                        Err(err) => ec.handle_error(err),
                    };
                    if let Err(err) = res {
                        log::error!("encode mysql time error={err}");
                        return (tp, None, false);
                    }
                    return (tp, Some(val), true);
                }
                return (ExprType::Null, None, false);
            }
            KindMysqlEnum => {
                tp = ExprType::MysqlEnum;
                encode_uint(&mut val, d.get_uint64());
            }
            KindVectorFloat32 => {
                tp = ExprType::TiDbVectorFloat32;
                val = d.get_vector_float32().zero_copy_serialize().to_vec();
            }
            _ => return (ExprType::Null, None, false),
        }
        (tp, Some(val), true)
    }
}

/// ToPBFieldType converts *types.FieldType to *tipb.FieldType.
///
/// tipb's `elems` are `string`s. v8.5.8 replaces invalid UTF-8 in ENUM / SET elements with
/// U+FFFD at CREATE TABLE (`enum(x'ff')` is stored as `EF BF BD`), so the elements are UTF-8
/// and the conversion gives Go's bytes; an element that is not (metadata written by another
/// version) is logged, asserted in debug builds and converted lossily (spec §4.2).
// go: ToPBFieldType L2 (non-UTF-8 elements, which v8.5.8 never writes, converted lossily)
pub fn to_pb_field_type(ft: &FieldType) -> tipb::FieldType {
    let mut pb = tipb::FieldType::default();
    pb.set_tp(ft.get_type() as i32);
    pb.set_flag(ft.get_flag() as u32);
    pb.set_flen(ft.get_flen() as i32);
    pb.set_decimal(ft.get_decimal() as i32);
    pb.set_charset(ft.get_charset().to_owned());
    pb.set_collate(collate::collation_to_proto(ft.get_collate()));
    if let Some(elems) = ft.get_elems() {
        pb.set_elems(RepeatedField::from_vec(
            elems.iter().map(|e| elem_to_pb(e)).collect(),
        ));
    }
    pb.set_array(false);
    pb
}

/// An ENUM / SET element as a tipb `string` (spec §4.2).
// glue: Go's string -> proto string needs no conversion
fn elem_to_pb(e: &[u8]) -> String {
    match std::str::from_utf8(e) {
        Ok(s) => s.to_owned(),
        Err(_) => {
            log::warn!(
                "ToPBFieldType: ENUM/SET element is not UTF-8, converted lossily elem={e:x?}"
            );
            debug_assert!(false, "ENUM/SET element is not UTF-8: {e:x?}");
            String::from_utf8_lossy(e).into_owned()
        }
    }
}

/// ToPBFieldTypeWithCheck converts *types.FieldType to *tipb.FieldType with checking the
/// valid decimal for TiFlash
// go: ToPBFieldTypeWithCheck L1
pub fn to_pb_field_type_with_check(
    ft: &FieldType,
    store_type: StoreType,
) -> Result<tipb::FieldType, Error> {
    if store_type == TiFlash && !ft.is_decimal_valid() {
        return Err(Error::plain(format!(
            "{} can not be pushed to TiFlash because it contains invalid decimal('{}','{}').",
            ft.to_string(),
            ft.get_flen(),
            ft.get_decimal()
        )));
    }
    Ok(to_pb_field_type(ft))
}

/// FieldTypeFromPB converts *tipb.FieldType to *types.FieldType.
///
/// Go's nil and empty `Elems` are one value after a protobuf round trip (nil); the Rust
/// message has only that one, so no elements is `None` (spec §4).
// go: FieldTypeFromPB L1 [M-INT]
pub fn field_type_from_pb(ft: &tipb::FieldType) -> FieldType {
    let mut ft1 = new_field_type_builder()
        .set_type(ft.get_tp() as u8)
        .set_flag(ft.get_flag() as usize)
        .set_flen(ft.get_flen() as isize)
        .set_decimal(ft.get_decimal() as isize)
        .set_charset(ft.get_charset())
        .set_collate(&collate::proto_to_collation(ft.get_collate()))
        .build_p();
    // A decoded `repeated string` with no element is nil in Go.
    if !ft.get_elems().is_empty() {
        ft1.set_elems(Some(
            ft.get_elems()
                .iter()
                .map(|e| e.as_bytes().to_vec())
                .collect(),
        ));
    }
    ft1
}

impl PbConverter<'_> {
    // go: PbConverter.columnToPBExpr L1
    pub(crate) fn column_to_pb_expr(
        &self,
        column: &Column,
        check_type: bool,
    ) -> Option<tipb::Expr> {
        if !self
            .client
            .is_request_type_supported(ReqTypeSelect, ExprType::ColumnRef as i64)
        {
            return None;
        }
        if check_type {
            match column.get_type().get_type() {
                mysql::TypeBit if !is_push_down_enabled(type_str(mysql::TypeBit), TiKV) => {
                    return None;
                }
                mysql::TypeSet | mysql::TypeGeometry | mysql::TypeUnspecified => return None,
                mysql::TypeEnum if !is_push_down_enabled("enum", UnSpecified) => return None,
                _ => {}
            }
        }

        if self
            .client
            .is_request_type_supported(ReqTypeDAG, ReqSubTypeBasic)
        {
            let mut val = Vec::new();
            encode_int(&mut val, column.index as i64);
            return pb_expr(
                ExprType::ColumnRef,
                Some(val),
                Unspecified,
                Vec::new(),
                Some(to_pb_field_type(&column.ret_type)),
            );
        }
        let id = column.id;
        // Zero Column ID is not a column from table, can not support for now.
        if id == 0 || id == -1 {
            return None;
        }

        let mut val = Vec::new();
        encode_int(&mut val, id);
        pb_expr(
            ExprType::ColumnRef,
            Some(val),
            Unspecified,
            Vec::new(),
            None,
        )
    }

    /// The failpoint `PanicIfPbCodeUnspecified` (tests only) is not ported.
    // go: PbConverter.scalarFuncToPBExpr L2 (test-only failpoint not ported)
    fn scalar_func_to_pb_expr(&self, expr: &ScalarFunction) -> Option<tipb::Expr> {
        // Check whether this function has ProtoBuf signature.
        let pb_code = expr.function.pb_code();
        if pb_code <= Unspecified {
            return None;
        }

        // Check whether this function can be pushed.
        if !can_func_be_pushed(self.ctx, expr, UnSpecified) {
            return None;
        }

        // Check whether all of its parameters can be pushed.
        let mut children = Vec::with_capacity(expr.get_args().len());
        for arg in expr.get_args() {
            let pb_arg = self.expr_to_pb(arg)?;
            children.push(pb_arg);
        }

        let mut encoded = None;
        if let Some(metadata) = expr.function.metadata() {
            let mut m = tipb::InUnionMetadata::default();
            m.set_in_union(metadata.in_union);
            match m.write_to_bytes() {
                Ok(b) => encoded = Some(b),
                Err(err) => {
                    log::error!("encode metadata metadata={metadata:?} error={err}");
                    return None;
                }
            }
        }

        // put collation information into the RetType enforcedly and push it down to TiKV/MockTiKV
        let mut tp = expr.ret_type().clone();
        if collate::new_collation_enabled() {
            let (_, str1) = expr.charset_and_collation();
            tp.set_collate(str1);
        }

        // Construct expression ProtoBuf.
        pb_expr(
            ExprType::ScalarFunc,
            encoded,
            pb_code,
            children,
            Some(to_pb_field_type(&tp)),
        )
    }
}

/// GroupByItemToPB converts group by items to pb.
// go: GroupByItemToPB L1
pub fn group_by_item_to_pb(
    ctx: &dyn EvalContext,
    st: &EvalState,
    client: &dyn Client,
    expr: &Expr,
) -> Option<ByItem> {
    let pc = PbConverter { client, ctx, st };
    let e = pc.expr_to_pb(expr)?;
    let mut item = ByItem::default();
    item.set_expr(e);
    item.set_desc(false);
    Some(item)
}

/// SortByItemToPB converts order by items to pb.
// go: SortByItemToPB L1
pub fn sort_by_item_to_pb(
    ctx: &dyn EvalContext,
    st: &EvalState,
    client: &dyn Client,
    expr: &Expr,
    desc: bool,
) -> Option<ByItem> {
    let pc = PbConverter { client, ctx, st };
    let e = pc.expr_to_pb(expr)?;
    let mut item = ByItem::default();
    item.set_expr(e);
    item.set_desc(desc);
    Some(item)
}
