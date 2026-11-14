//! go-file: pkg/expression/distsql_builtin.go
//!
//! The reverse direction of `expr_to_pb.go`: `tipb.Expr` to expressions (spec
//! `specs/D08.3-pushdown` §6). Used where tidb-rs evaluates a pushed-down expression itself
//! (a coprocessor request sent to the TiDB store type).
//!
//! - `getSignatureByPB` is Go's `switch sigCode` with one case per pb code (559 codes). The
//!   cases are the table `PB_SIGS`, generated from the Go source by
//!   `tools/expr-gen/gen_pb_sigs.py`: the Go signature type of each struct literal and how the
//!   literal fills the signature's fields beyond `baseBuiltinFunc` ([`Ctor`]). The signature
//!   is the matching `SigDesc` of tidb-expression (`PORTED`, generated too).
//! - **Boundary stubs** (`blocked-by`, constitution §4.3): the codes whose signature type
//!   belongs to a function family that is not ported yet (D08.6 string, D08.7 time, D08.9
//!   misc, D08.10 JSON; the generated table names the block of each) return an "is not
//!   ported yet (<block>)" error. A stub disappears when its family adds its `SigDesc` and
//!   the table is regenerated.
//! - Go returns `nil` expressions only together with an error; Rust returns `Result`.

// Go identifiers for constants and statics (mapping rule M-GONAME).
#![allow(non_upper_case_globals)]

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use protobuf::{Message, ProtobufEnum};
use tidb_base::Error;
use tidb_base::gofmt::Arg;
use tidb_base::gostd::time::{Location, utc};
use tidb_base::mysql;
use tidb_codec::codec::{decode, decode_one};
use tidb_codec::decimal::decode_decimal;
use tidb_codec::float::decode_float;
use tidb_codec::number::{decode_int, decode_uint};
use tidb_expression::builtin::{new_base_builtin_cast_func, new_base_builtin_func_with_field_type};
use tidb_expression::eval::{BuiltinFunc, SigDesc, SigExtra};
use tidb_expression::{
    BuildContext, Column, Constant, ErrFunctionNotExists, Expr, ExprRef, ScalarFunction,
};
use tidb_types::binary_literal::BinaryLiteral;
use tidb_types::collate;
use tidb_types::datum::{
    Datum, KindMysqlJSON, new_bytes_datum, new_int_datum, new_mysql_bit_datum,
    new_mysql_enum_datum, new_time_datum,
};
use tidb_types::enum_type::{Enum, parse_enum_value};
use tidb_types::etc::FieldType;
use tidb_types::field_type::new_field_type;
use tidb_types::field_type_builder::new_field_type_builder;
use tidb_types::fsp::MaxFsp;
use tidb_types::time::{Duration, Time};
use tidb_types::vector::zero_copy_deserialize_vector_float32;
use tipb::ExprType;

use crate::expr_to_pb::field_type_from_pb;
use crate::tipb_sig::scalar_func_sig_string;

#[path = "distsql_builtin_gen.rs"]
mod distsql_builtin_gen;
use distsql_builtin_gen::{PB_SIGS, PORTED};

/// How a case of `getSignatureByPB` fills the signature's fields beyond `baseBuiltinFunc`
/// (Go's struct literal; tidb-expression keeps them in [`SigExtra`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Ctor {
    /// `&builtinXxxSig{base}` (also the literals that only nest `base`).
    Base,
    /// `&builtinCastXxxSig{newBaseBuiltinCastFunc(base, inUnion)}`.
    Cast { in_union: bool },
    /// `&builtinXxxSig{base, maxAllowedPacket}` (D08.6).
    MaxAllowedPacket,
    /// `cmpAsDate` of GREATEST / LEAST.
    Extremum(bool),
    /// `hasNullable` of INTERVAL.
    IntervalInt(bool),
    /// `hasNullable` of INTERVAL.
    IntervalReal(bool),
    /// `constantArgOverflow` of unary minus.
    UnaryMinusDecimal(bool),
    /// `keepNull` of IS TRUE / IS FALSE.
    IsTrueOrFalse(bool),
    /// `cannotConvertStringAsWarning` of `from_binary`.
    FromBinary(bool),
    /// `addSubDateFunctionClass{...}.getFunction(ctx, args)` (D08.7).
    AddSubDate { sub: bool },
}

/// The case of each pb code, and the ported signatures by (Go type, pb code) and by Go type.
struct PbSigIndex {
    cases: HashMap<i32, (&'static str, Ctor, &'static str)>,
    by_code: HashMap<(&'static str, i32), &'static SigDesc>,
    by_name: HashMap<&'static str, Vec<&'static SigDesc>>,
}

static INDEX: LazyLock<PbSigIndex> = LazyLock::new(|| {
    let mut idx = PbSigIndex {
        cases: HashMap::new(),
        by_code: HashMap::new(),
        by_name: HashMap::new(),
    };
    for &(code, go, ctor, block) in PB_SIGS {
        idx.cases.insert(code, (go, ctor, block));
    }
    for &sig in PORTED {
        idx.by_code.insert((sig.name, sig.pb_code), sig);
        idx.by_name.entry(sig.name).or_default().push(sig);
    }
    idx
});

/// The `SigDesc` of Go type `go` for `code`: the one with that pb code (GREATEST(date) and
/// GREATEST(datetime) are one Go type with two codes), else the only one of that type (the
/// IS TRUE / IS FALSE `WithNull` codes are set after the plain signature is built).
// glue: Go's struct literal names the type; tidb-expression has one SigDesc per (type, code)
fn ported_sig(go: &str, code: i32) -> Option<&'static SigDesc> {
    if let Some(&s) = INDEX.by_code.get(&(go, code)) {
        return Some(s);
    }
    match INDEX.by_name.get(go).map(Vec::as_slice) {
        Some([s]) => Some(*s),
        _ => None,
    }
}

/// The boundary stub of a signature whose family is not ported yet.
// glue: boundary stub (blocked-by:D08.6, D08.7, D08.9, D08.10; constitution §4.3)
fn not_ported(go: &str, code: i32, block: &str) -> Error {
    Error::plain(format!(
        "signature {go} (pb code {}) is not ported yet ({block})",
        scalar_func_sig_string(code)
    ))
}

/// PbTypeToFieldType converts tipb.FieldType to FieldType
// go: PbTypeToFieldType L1 [M-INT]
pub fn pb_type_to_field_type(tp: &tipb::FieldType) -> FieldType {
    let ftb = new_field_type_builder();
    let mut ft = ftb
        .set_type(tp.get_tp() as u8)
        .set_flag(tp.get_flag() as usize)
        .set_flen(tp.get_flen() as isize)
        .set_decimal(tp.get_decimal() as isize)
        .set_charset(tp.get_charset())
        .set_collate(&collate::proto_to_collation(tp.get_collate()))
        .build_p();
    // A decoded `repeated string` with no element is nil in Go (spec §4.3).
    if !tp.get_elems().is_empty() {
        ft.set_elems(Some(
            tp.get_elems()
                .iter()
                .map(|e| e.as_bytes().to_vec())
                .collect(),
        ));
    }
    ft
}

/// The cases of Go's `switch sigCode` are the generated table (module docs); the signatures of
/// families that are not ported yet are boundary stubs.
// go: getSignatureByPB L2 (switch as a generated table; unported families are boundary stubs)
fn get_signature_by_pb(
    ctx: &dyn BuildContext,
    sig_code: i32,
    tp: &tipb::FieldType,
    args: Vec<ExprRef>,
) -> Result<BuiltinFunc, Error> {
    let field_tp = pb_type_to_field_type(tp);
    let base = new_base_builtin_func_with_field_type(field_tp, args)?;
    // maxAllowedPacket: read by the D08.6 literals (stubs until D08.6).
    let _max_allowed_packet = ctx.get_eval_ctx().get_max_allowed_packet();
    let Some(&(go, ctor, block)) = INDEX.cases.get(&sig_code) else {
        return Err(ErrFunctionNotExists.gen_with_stack_by_args(vec![
            Arg::from("FUNCTION"),
            Arg::from(scalar_func_sig_string(sig_code)),
        ]));
    };
    let Some(sig) = ported_sig(go, sig_code) else {
        return Err(not_ported(go, sig_code, block));
    };
    let mut f = base.with_sig(sig);
    match ctor {
        Ctor::Base => {}
        Ctor::Cast { in_union } => f = new_base_builtin_cast_func(f, in_union),
        Ctor::Extremum(cmp_as_date) => f.extra = SigExtra::Extremum { cmp_as_date },
        Ctor::IntervalInt(has_nullable) => f.extra = SigExtra::IntervalInt { has_nullable },
        Ctor::IntervalReal(has_nullable) => f.extra = SigExtra::IntervalReal { has_nullable },
        Ctor::UnaryMinusDecimal(constant_arg_overflow) => {
            f.extra = SigExtra::UnaryMinusDecimal {
                constant_arg_overflow,
            }
        }
        Ctor::IsTrueOrFalse(keep_null) => f.extra = SigExtra::IsTrueOrFalse { keep_null },
        Ctor::FromBinary(cannot_convert_string_as_warning) => {
            // TODO: set the `cannotConvertStringAsWarning` accordingly
            f.extra = SigExtra::FromBinary {
                cannot_convert_string_as_warning,
            }
        }
        // The fields of these literals come with their families (D08.6, D08.7).
        Ctor::MaxAllowedPacket | Ctor::AddSubDate { .. } => {
            return Err(not_ported(go, sig_code, block));
        }
    }
    f.set_pb_code(sig_code);
    Ok(f)
}

// go: newDistSQLFunctionBySig L1
fn new_dist_sql_function_by_sig(
    ctx: &dyn BuildContext,
    sig_code: i32,
    tp: &tipb::FieldType,
    args: Vec<ExprRef>,
) -> Result<ExprRef, Error> {
    let f = get_signature_by_pb(ctx, sig_code, tp, args)?;
    // fmt.Sprintf("sig_%T", f): the Go type of the signature.
    let name = format!("sig_*expression.{}", f.sig.name);
    let ret_type = f.tp.clone();
    Ok(Expr::ScalarFunction(ScalarFunction::new(&name, ret_type, f)).into_ref())
}

/// PBToExprs converts pb structures to expressions.
// go: PBToExprs L1
pub fn pb_to_exprs(
    ctx: &dyn BuildContext,
    pb_exprs: &[tipb::Expr],
    field_tps: &[FieldType],
) -> Result<Vec<ExprRef>, Error> {
    let mut exprs = Vec::with_capacity(pb_exprs.len());
    for expr in pb_exprs {
        // Go also fails when PBToExpr returns a nil expression without an error, which it
        // never does.
        let e = pb_to_expr(ctx, expr, field_tps)?;
        exprs.push(e);
    }
    Ok(exprs)
}

/// The value of an enum field as Go sees it: Go keeps a value its tipb does not know, the
/// Rust message keeps it in the unknown fields (`None` when the field is absent).
// glue: proto2 enum values unknown to the Rust tipb crate
fn raw_enum(m: &tipb::Expr, known: Option<i32>, field: u32) -> Option<i32> {
    if known.is_some() {
        return known;
    }
    let mut v = None;
    for (n, values) in m.get_unknown_fields() {
        if n == field {
            v = values.varint.last().map(|&x| x as i32);
        }
    }
    v
}

/// Go dereferences a nil `FieldType` of a constant or function.
// glue: Go's nil *tipb.FieldType dereference
fn field_type(expr: &tipb::Expr) -> &tipb::FieldType {
    if !expr.has_field_type() {
        panic!("runtime error: invalid memory address or nil pointer dereference");
    }
    expr.get_field_type()
}

/// Go's `expr.Val`: nil when absent.
// glue: Go's []byte field (nil when absent)
fn val(expr: &tipb::Expr) -> Option<Vec<u8>> {
    expr.has_val().then(|| expr.get_val().to_vec())
}

/// PBToExpr converts pb structure to expression.
///
/// An expression type or signature the Rust tipb crate does not know is read from the
/// message's unknown fields, as Go keeps it in the field.
// go: PBToExpr L1
pub fn pb_to_expr(
    ctx: &dyn BuildContext,
    expr: &tipb::Expr,
    tps: &[FieldType],
) -> Result<ExprRef, Error> {
    let eval_ctx = ctx.get_eval_ctx();
    let tp = raw_enum(expr, expr.has_tp().then(|| expr.get_tp() as i32), 1).unwrap_or(0);
    let v = expr.get_val();
    let c = |r: Result<Constant, Error>| r.map(|c| Expr::Constant(c).into_ref());
    match ExprType::from_i32(tp) {
        Some(ExprType::ColumnRef) => {
            let (_, offset) = decode_int(v)?;
            return Ok(Expr::Column(Column {
                index: offset as isize,
                ret_type: tps[offset as usize].clone(),
                ..Column::default()
            })
            .into_ref());
        }
        Some(ExprType::Null) => {
            return c(Ok(Constant::new(
                Datum::default(),
                new_field_type(mysql::TypeNull),
            )));
        }
        Some(ExprType::Int64) => return c(convert_int(v, field_type(expr))),
        Some(ExprType::Uint64) => return c(convert_uint(v, field_type(expr))),
        Some(ExprType::String) => return c(convert_string(v, field_type(expr))),
        Some(ExprType::Bytes) => {
            return c(Ok(Constant::new(
                new_bytes_datum(val(expr)),
                new_field_type(mysql::TypeString),
            )));
        }
        Some(ExprType::MysqlBit) => {
            return c(Ok(Constant::new(
                new_mysql_bit_datum(BinaryLiteral(v.to_vec())),
                new_field_type(mysql::TypeString),
            )));
        }
        Some(ExprType::Float32) => return c(convert_float(v, true)),
        Some(ExprType::Float64) => return c(convert_float(v, false)),
        Some(ExprType::MysqlDecimal) => return c(convert_decimal(v, field_type(expr))),
        Some(ExprType::MysqlDuration) => return c(convert_duration(v)),
        Some(ExprType::MysqlTime) => {
            return c(convert_time(v, field_type(expr), &eval_ctx.location()));
        }
        Some(ExprType::MysqlJson) => return c(convert_json(v)),
        Some(ExprType::MysqlEnum) => return c(convert_enum(v, field_type(expr))),
        Some(ExprType::TiDbVectorFloat32) => return c(convert_vector_float32(v)),
        _ => {}
    }
    if tp != ExprType::ScalarFunc as i32 {
        panic!("should be a tipb.ExprType_ScalarFunc");
    }
    // Then it must be a scalar function.
    let mut args = Vec::with_capacity(expr.get_children().len());
    for child in expr.get_children() {
        if raw_enum(child, child.has_tp().then(|| child.get_tp() as i32), 1)
            == Some(ExprType::ValueList as i32)
        {
            let results = decode_value_list(child.get_val())?;
            if results.is_empty() {
                return c(Ok(Constant::new(
                    new_int_datum(0),
                    new_field_type(mysql::TypeLonglong),
                )));
            }
            args.extend(results);
            continue;
        }
        let arg = pb_to_expr(ctx, child, tps)?;
        args.push(arg);
    }
    let sig = raw_enum(expr, expr.has_sig().then(|| expr.get_sig() as i32), 4).unwrap_or(0);
    new_dist_sql_function_by_sig(ctx, sig, field_type(expr), args)
}

// go: convertTime L1
fn convert_time(
    data: &[u8],
    ft_pb: &tipb::FieldType,
    tz: &Arc<Location>,
) -> Result<Constant, Error> {
    let ft = pb_type_to_field_type(ft_pb);
    let (_, v) = decode_uint(data)?;
    let mut t = Time::default();
    t.set_type(ft.get_type());
    t.set_fsp(ft.get_decimal());
    t.from_packed_uint(v)?;
    if ft.get_type() == mysql::TypeTimestamp && !Arc::ptr_eq(tz, &utc()) {
        t.convert_time_zone(&utc(), tz)?;
    }
    Ok(Constant::new(new_time_datum(t), ft))
}

/// Go's `&Constant{Value: value}` has a nil `RetType`; the Rust constant has the zero
/// `FieldType` (spec §6.2). Nothing reads it before the signature is built.
// go: decodeValueList L2 (nil RetType is the zero FieldType)
fn decode_value_list(data: &[u8]) -> Result<Vec<ExprRef>, Error> {
    if data.is_empty() {
        return Ok(Vec::new());
    }
    let list = decode(data, 1)?;
    let mut result = Vec::with_capacity(list.len());
    for value in list {
        result.push(Expr::Constant(Constant::new(value, FieldType::default())).into_ref());
    }
    Ok(result)
}

/// Go's `% x`: the bytes in lower-case hex separated by spaces.
// go-std: fmt verb "% x" L1
fn space_hex(b: &[u8]) -> String {
    b.iter()
        .map(|x| format!("{x:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

// go: convertInt L1
fn convert_int(val: &[u8], tp: &tipb::FieldType) -> Result<Constant, Error> {
    let mut d = Datum::default();
    let Ok((_, i)) = decode_int(val) else {
        return Err(Error::plain(format!("invalid int {}", space_hex(val))));
    };
    d.set_int64(i);
    Ok(Constant::new(d, pb_type_to_field_type(tp)))
}

// go: convertUint L1
fn convert_uint(val: &[u8], tp: &tipb::FieldType) -> Result<Constant, Error> {
    let mut d = Datum::default();
    let Ok((_, u)) = decode_uint(val) else {
        return Err(Error::plain(format!("invalid uint {}", space_hex(val))));
    };
    d.set_uint64(u);
    Ok(Constant::new(d, pb_type_to_field_type(tp)))
}

// go: convertString L1 [M-INT]
fn convert_string(val: &[u8], tp: &tipb::FieldType) -> Result<Constant, Error> {
    let mut d = Datum::default();
    d.set_bytes_as_string(
        val.to_vec(),
        collate::proto_to_collation(tp.get_collate()),
        tp.get_flen() as u32,
    );
    Ok(Constant::new(d, pb_type_to_field_type(tp)))
}

// go: convertFloat L1
fn convert_float(val: &[u8], f32: bool) -> Result<Constant, Error> {
    let mut d = Datum::default();
    let Ok((_, f)) = decode_float(val) else {
        return Err(Error::plain(format!("invalid float {}", space_hex(val))));
    };
    if f32 {
        d.set_float32(f as f32);
    } else {
        d.set_float64(f);
    }
    Ok(Constant::new(d, new_field_type(mysql::TypeDouble)))
}

// go: convertDecimal L1
fn convert_decimal(val: &[u8], ft_pb: &tipb::FieldType) -> Result<Constant, Error> {
    let ft = pb_type_to_field_type(ft_pb);
    let (_, dec, precision, frac, res) = decode_decimal(val);
    let mut d = Datum::default();
    if res.is_err() {
        return Err(Error::plain(format!("invalid decimal {}", space_hex(val))));
    }
    d.set_mysql_decimal(dec.unwrap_or_default());
    d.set_length(precision);
    d.set_frac(frac);
    Ok(Constant::new(d, ft))
}

// go: convertDuration L1
fn convert_duration(val: &[u8]) -> Result<Constant, Error> {
    let mut d = Datum::default();
    let i = match decode_int(val) {
        Ok((_, i)) => i,
        // Go prints the zero value DecodeInt returns with the error.
        Err(_) => return Err(Error::plain("invalid duration 0")),
    };
    d.set_mysql_duration(Duration {
        duration: i,
        fsp: MaxFsp,
    });
    Ok(Constant::new(d, new_field_type(mysql::TypeDuration)))
}

// go: convertJSON L1
fn convert_json(val: &[u8]) -> Result<Constant, Error> {
    let (_, d, res) = decode_one(val);
    if res.is_err() {
        return Err(Error::plain(format!("invalid json {}", space_hex(val))));
    }
    if d.kind() != KindMysqlJSON {
        return Err(Error::plain(format!("invalid Datum.Kind() {}", d.kind())));
    }
    Ok(Constant::new(d, new_field_type(mysql::TypeJSON)))
}

// go: convertVectorFloat32 L1
fn convert_vector_float32(val: &[u8]) -> Result<Constant, Error> {
    let Ok((v, _)) = zero_copy_deserialize_vector_float32(val) else {
        return Err(Error::plain(format!(
            "invalid VectorFloat32 {}",
            val.iter().map(|x| format!("{x:02x}")).collect::<String>()
        )));
    };
    let mut d = Datum::default();
    d.set_vector_float32(v);
    Ok(Constant::new(
        d,
        new_field_type(mysql::TypeTiDBVectorFloat32),
    ))
}

// go: convertEnum L1
fn convert_enum(val: &[u8], tp: &tipb::FieldType) -> Result<Constant, Error> {
    let Ok((_, u_val)) = decode_uint(val) else {
        return Err(Error::plain(format!("invalid enum {}", space_hex(val))));
    };
    // If uVal is 0, it should return Enum{}
    let mut e = Enum::default();
    if u_val != 0 {
        let elems: Vec<Vec<u8>> = tp
            .get_elems()
            .iter()
            .map(|s| s.as_bytes().to_vec())
            .collect();
        e = parse_enum_value(&elems, u_val)?;
    }
    let d = new_mysql_enum_datum(e);
    Ok(Constant::new(d, field_type_from_pb(tp)))
}
