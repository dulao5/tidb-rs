//! go-file: pkg/expression/constant.go
//!
//! `Constant`, `ParamMarker` and the `New*` constructors (spec §3).
//!
//! - The cached `hashcode` is a `OnceLock` (N4 §3.5).
//! - Not ported (spec §4): `VecEval*` (vector driver of part 2, M-EVAL) and the
//!   cascades-only `Hash64`/`Equals` (EXC.B).
//! - `intest.AssertNoError`/`AssertNotNil` are `debug_assert!`s.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use tidb_base::errors::{RedactLogDisable, RedactLogMarker};
use tidb_base::{Error, mysql};
use tidb_chunk::Row;
use tidb_types::collate::get_binary_collator;
use tidb_types::datum::{
    Datum, KindBinaryLiteral, KindMysqlBit, KindMysqlDecimal, KindString, new_int_datum,
    new_string_datum, new_uint_datum,
};
use tidb_types::etc::FieldType;
use tidb_types::field_type::{infer_param_type_from_datum, new_field_type};
use tidb_types::json::BinaryJSON;
use tidb_types::mydecimal::{MODE_HALF_UP, MyDecimal};
use tidb_types::time::{Duration, Time};
use tidb_types::vector::VectorFloat32;

use crate::collation::{Coercibility, CollationInfo, derive_coercibility_for_constant};
use crate::column::Column;
use crate::context::{EvalContext, ParamValues, type_ctx};
use crate::expression::{
    ConstLevel, ConstOnlyInContext, ConstStrict, EvalState, Expr, ExprRef, TraverseAction,
    constantFlag, parameterFlag,
};
use crate::schema::Schema;

/// NewOne stands for a unsigned number 1.
// go: NewOne L1
pub fn new_one() -> Constant {
    let mut ret_t = new_field_type(mysql::TypeTiny);
    ret_t.add_flag(mysql::UnsignedFlag); // shrink range to avoid integral promotion
    ret_t.set_flen(1);
    ret_t.set_decimal(0);
    Constant::new(new_int_datum(1), ret_t)
}

/// NewSignedOne stands for a signed number 1.
// go: NewSignedOne L1
pub fn new_signed_one() -> Constant {
    let mut ret_t = new_field_type(mysql::TypeTiny);
    ret_t.set_flen(1);
    ret_t.set_decimal(0);
    Constant::new(new_int_datum(1), ret_t)
}

/// NewZero stands for a unsigned number 0.
// go: NewZero L1
pub fn new_zero() -> Constant {
    let mut ret_t = new_field_type(mysql::TypeTiny);
    ret_t.add_flag(mysql::UnsignedFlag); // shrink range to avoid integral promotion
    ret_t.set_flen(1);
    ret_t.set_decimal(0);
    Constant::new(new_int_datum(0), ret_t)
}

/// NewSignedZero stands for a signed number 0.
// go: NewSignedZero L1
pub fn new_signed_zero() -> Constant {
    let mut ret_t = new_field_type(mysql::TypeTiny);
    ret_t.set_flen(1);
    ret_t.set_decimal(0);
    Constant::new(new_int_datum(0), ret_t)
}

/// NewUInt64Const stands for constant of a given number.
// go: NewUInt64Const L1
pub fn new_uint64_const(num: isize) -> Constant {
    let mut ret_t = new_field_type(mysql::TypeLonglong);
    ret_t.add_flag(mysql::UnsignedFlag); // shrink range to avoid integral promotion
    ret_t.set_flen(mysql::MaxIntWidth);
    ret_t.set_decimal(0);
    // types.NewDatum(num) with a Go int: a KindInt64 datum.
    Constant::new(new_int_datum(num as i64), ret_t)
}

/// NewUInt64ConstWithFieldType stands for constant of a given number with specified fieldType.
// go: NewUInt64ConstWithFieldType L1
pub fn new_uint64_const_with_field_type(num: u64, field_type: FieldType) -> Constant {
    Constant::new(new_uint_datum(num), field_type)
}

/// NewInt64Const stands for constant of a given number.
// go: NewInt64Const L1
pub fn new_int64_const(num: i64) -> Constant {
    let mut ret_t = new_field_type(mysql::TypeLonglong);
    ret_t.set_flen(mysql::MaxIntWidth);
    ret_t.set_decimal(0);
    Constant::new(new_int_datum(num), ret_t)
}

/// NewStrConst stands for constant of a given string.
/// used in test only now.
// go: NewStrConst L1
pub fn new_str_const(s: &[u8]) -> Constant {
    let mut ret_t = new_field_type(mysql::TypeVarString);
    ret_t.set_flen(s.len() as isize);
    Constant::new(new_string_datum(s.to_vec()), ret_t)
}

/// NewNull stands for null constant.
// go: NewNull L1
pub fn new_null() -> Constant {
    let mut ret_t = new_field_type(mysql::TypeTiny);
    ret_t.set_flen(1);
    ret_t.set_decimal(0);
    Constant::new(Datum::default(), ret_t)
}

/// NewNullWithFieldType stands for null constant with specified fieldType.
// go: NewNullWithFieldType L1
pub fn new_null_with_field_type(field_type: FieldType) -> Constant {
    Constant::new(Datum::default(), field_type)
}

/// Constant stands for a constant value.
#[derive(Debug)]
pub struct Constant {
    /// Value.
    pub value: Datum,
    /// RetType.
    pub ret_type: FieldType,
    /// DeferredExpr holds deferred function in PlanCache cached plan.
    /// it's only used to represent non-deterministic functions(see expression.DeferredFunctions)
    /// in PlanCache cached plan, so let them can be evaluated until cached item be used.
    pub deferred_expr: Option<ExprRef>,
    /// ParamMarker holds param index inside sessionVars.PreparedParams.
    /// It's only used to reference a user variable provided in the `EXECUTE` statement or `COM_EXECUTE` binary protocol.
    pub param_marker: Option<ParamMarker>,
    hashcode: OnceLock<Vec<u8>>,

    /// The embedded `collationInfo`.
    pub collation_info: CollationInfo,
}

/// ParamMarker indicates param provided by COM_STMT_EXECUTE.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParamMarker {
    /// order.
    pub order: i64,
}

impl ParamMarker {
    /// GetUserVar returns the corresponding user variable presented in the `EXECUTE` statement or `COM_EXECUTE` command.
    // go: ParamMarker.GetUserVar L1
    pub fn get_user_var(&self, ctx: &dyn ParamValues) -> Result<Datum, Error> {
        ctx.get_param_value(self.order)
    }
}

impl Clone for Constant {
    /// Clone implements Expression interface.
    // go: Constant.Clone L1
    fn clone(&self) -> Self {
        Constant {
            value: self.value.clone(),
            ret_type: self.ret_type.clone(),
            param_marker: self.param_marker.map(|p| ParamMarker { order: p.order }),
            // Go clones the deferred expression; nodes are immutable and shared here (M-EXPR).
            deferred_expr: self.deferred_expr.clone(),
            hashcode: self.hashcode.clone(),
            collation_info: self.collation_info.clone(),
        }
    }
}

impl Constant {
    /// `&Constant{Value: v, RetType: t}`.
    // glue: Go composite literal
    pub fn new(value: Datum, ret_type: FieldType) -> Constant {
        Constant {
            value,
            ret_type,
            deferred_expr: None,
            param_marker: None,
            hashcode: OnceLock::new(),
            collation_info: CollationInfo::default(),
        }
    }

    /// StringWithCtx implements Expression interface.
    // go: Constant.StringWithCtx L1
    pub fn string_with_ctx(&self, ctx: &dyn ParamValues, redact: &str) -> Vec<u8> {
        let mut v = Cow::Borrowed(&self.value);
        if let Some(pm) = &self.param_marker {
            // intest.AssertNoError: compiled out of production Go builds (also here).
            let dt = pm.get_user_var(ctx);
            match dt {
                Err(_) => return b"?".to_vec(),
                Ok(dt) => v = Cow::Owned(dt),
            }
        } else if let Some(de) = &self.deferred_expr {
            return de.string_with_ctx(ctx, redact);
        }
        if redact == RedactLogDisable {
            return v.truncated_stringify();
        } else if redact == RedactLogMarker {
            let mut out = "‹".as_bytes().to_vec();
            out.extend_from_slice(&v.truncated_stringify());
            out.extend_from_slice("›".as_bytes());
            return out;
        }
        b"?".to_vec()
    }

    /// GetType implements Expression interface. `None` is Go's nil `*FieldType` (the
    /// parameter cannot be read; Go logs a warning).
    // go: Constant.GetType L1
    pub fn get_type(&self, ctx: &dyn EvalContext) -> Option<Cow<'_, FieldType>> {
        if let Some(pm) = &self.param_marker {
            let mut tp = new_field_type(mysql::TypeUnspecified);
            // intest.AssertNoError: compiled out of production Go builds (also here).
            let Ok(dt) = pm.get_user_var(ctx) else {
                return None;
            };
            infer_param_type_from_datum(&dt, &mut tp);
            return Some(Cow::Owned(tp));
        }
        Some(Cow::Borrowed(&self.ret_type))
    }

    /// `c.GetType(ctx)` where Go dereferences the result.
    // go-part: Constant.GetType
    fn ty(&self, ctx: &dyn EvalContext) -> Cow<'_, FieldType> {
        self.get_type(ctx)
            .expect("nil *FieldType: the parameter cannot be read")
    }

    // go: Constant.getLazyDatum L1
    fn get_lazy_datum(
        &self,
        ctx: &dyn EvalContext,
        st: &EvalState,
        row: Row<'_>,
    ) -> (Option<Datum>, Result<(), Error>) {
        if let Some(pm) = &self.param_marker {
            // intest.AssertNoError: compiled out of production Go builds (also here).
            let val = pm.get_user_var(ctx);
            return match val {
                Err(e) => (Some(Datum::default()), Err(e)),
                Ok(v) => (Some(v), Ok(())),
            };
        } else if let Some(de) = &self.deferred_expr {
            return match de.eval(ctx, st, row) {
                Ok(dt) => (Some(dt), Ok(())),
                Err(e) => (Some(Datum::default()), Err(e)),
            };
        }
        (None, Ok(()))
    }

    /// `dt, lazy, err := c.getLazyDatum(ctx, row); if err != nil {...}; if !lazy { dt = c.Value }`.
    // go-part: Constant.getLazyDatum
    fn datum_for_eval(
        &self,
        ctx: &dyn EvalContext,
        st: &EvalState,
        row: Row<'_>,
    ) -> Result<Cow<'_, Datum>, Error> {
        let (dt, r) = self.get_lazy_datum(ctx, st, row);
        r?;
        Ok(match dt {
            Some(dt) => Cow::Owned(dt),
            None => Cow::Borrowed(&self.value),
        })
    }

    /// Traverse implements the TraverseDown interface.
    // go: Constant.Traverse L1
    pub fn traverse(&self, this: &ExprRef, action: &mut dyn TraverseAction) -> ExprRef {
        action.transform(this)
    }

    /// Eval implements Expression interface.
    // go: Constant.Eval L1
    pub fn eval(
        &self,
        ctx: &dyn EvalContext,
        st: &EvalState,
        row: Row<'_>,
    ) -> Result<Datum, Error> {
        let (dt, r) = self.get_lazy_datum(ctx, st, row);
        if let Some(mut dt) = dt {
            r?;
            if dt.is_null() {
                return Ok(dt);
            }
            if self.deferred_expr.is_some() {
                if dt.kind() != KindMysqlDecimal {
                    let (val, r) = dt.convert_to(&type_ctx(ctx), &self.ret_type);
                    r?;
                    return Ok(val);
                }
                let mut d = *dt.get_mysql_decimal();
                self.adjust_decimal(ctx, &mut d)?;
                dt.set_mysql_decimal(d);
            }
            return Ok(dt);
        }
        Ok(self.value.clone())
    }

    /// EvalInt returns int representation of Constant.
    // go: Constant.EvalInt L1
    pub fn eval_int(
        &self,
        ctx: &dyn EvalContext,
        st: &EvalState,
        row: Row<'_>,
    ) -> Result<Option<i64>, Error> {
        let dt = self.datum_for_eval(ctx, st, row)?;
        if self.ty(ctx).get_type() == mysql::TypeNull || dt.is_null() {
            return Ok(None);
        } else if dt.kind() == KindBinaryLiteral {
            let (val, r) = dt.get_binary_literal().to_int(&type_ctx(ctx));
            r?;
            return Ok(Some(val as i64));
        } else if self.ty(ctx).hybrid() || dt.kind() == KindString {
            let (res, r) = dt.to_int64(&type_ctx(ctx));
            r?;
            return Ok(Some(res));
        } else if dt.kind() == KindMysqlBit {
            let (uint_val, r) = dt.get_binary_literal().to_int(&type_ctx(ctx));
            r?;
            return Ok(Some(uint_val as i64));
        }
        Ok(Some(dt.get_int64()))
    }

    /// EvalReal returns real representation of Constant.
    // go: Constant.EvalReal L1
    pub fn eval_real(
        &self,
        ctx: &dyn EvalContext,
        st: &EvalState,
        row: Row<'_>,
    ) -> Result<Option<f64>, Error> {
        let dt = self.datum_for_eval(ctx, st, row)?;
        if self.ty(ctx).get_type() == mysql::TypeNull || dt.is_null() {
            return Ok(None);
        }
        if self.ty(ctx).hybrid() || dt.kind() == KindBinaryLiteral || dt.kind() == KindString {
            let (res, r) = dt.to_float64(&type_ctx(ctx));
            r?;
            return Ok(Some(res));
        }
        Ok(Some(dt.get_float64()))
    }

    /// EvalString returns string representation of Constant.
    // go: Constant.EvalString L1
    pub fn eval_string(
        &self,
        ctx: &dyn EvalContext,
        st: &EvalState,
        row: Row<'_>,
    ) -> Result<Option<Vec<u8>>, Error> {
        let dt = self.datum_for_eval(ctx, st, row)?;
        if self.ty(ctx).get_type() == mysql::TypeNull || dt.is_null() {
            return Ok(None);
        }
        dt.to_string().map(Some)
    }

    /// EvalDecimal returns decimal representation of Constant.
    // go: Constant.EvalDecimal L1
    pub fn eval_decimal(
        &self,
        ctx: &dyn EvalContext,
        st: &EvalState,
        row: Row<'_>,
    ) -> Result<Option<MyDecimal>, Error> {
        let dt = self.datum_for_eval(ctx, st, row)?;
        if self.ty(ctx).get_type() == mysql::TypeNull || dt.is_null() {
            return Ok(None);
        }
        let (mut res, r) = dt.to_decimal(&type_ctx(ctx));
        r?;
        self.adjust_decimal(ctx, &mut res)?;
        Ok(Some(res))
    }

    // go: Constant.adjustDecimal L1
    fn adjust_decimal(&self, ctx: &dyn EvalContext, d: &mut MyDecimal) -> Result<(), Error> {
        // Decimal Value's precision and frac may be modified during plan building.
        let (_, frac) = d.precision_and_frac();
        if frac < self.ty(ctx).get_decimal() {
            return d.round_in_place(self.ty(ctx).get_decimal(), MODE_HALF_UP);
        }
        Ok(())
    }

    /// EvalTime returns DATE/DATETIME/TIMESTAMP representation of Constant.
    // go: Constant.EvalTime L1
    pub fn eval_time(
        &self,
        ctx: &dyn EvalContext,
        st: &EvalState,
        row: Row<'_>,
    ) -> Result<Option<Time>, Error> {
        let dt = self.datum_for_eval(ctx, st, row)?;
        if self.ty(ctx).get_type() == mysql::TypeNull || dt.is_null() {
            return Ok(None);
        }
        Ok(Some(dt.get_mysql_time()))
    }

    /// EvalDuration returns Duration representation of Constant.
    // go: Constant.EvalDuration L1
    pub fn eval_duration(
        &self,
        ctx: &dyn EvalContext,
        st: &EvalState,
        row: Row<'_>,
    ) -> Result<Option<Duration>, Error> {
        let dt = self.datum_for_eval(ctx, st, row)?;
        if self.ty(ctx).get_type() == mysql::TypeNull || dt.is_null() {
            return Ok(None);
        }
        Ok(Some(dt.get_mysql_duration()))
    }

    /// EvalJSON returns JSON representation of Constant.
    // go: Constant.EvalJSON L1
    pub fn eval_json(
        &self,
        ctx: &dyn EvalContext,
        st: &EvalState,
        row: Row<'_>,
    ) -> Result<Option<BinaryJSON<'static>>, Error> {
        let dt = self.datum_for_eval(ctx, st, row)?;
        if self.ty(ctx).get_type() == mysql::TypeNull || dt.is_null() {
            return Ok(None);
        }
        Ok(Some(dt.get_mysql_json().into_owned()))
    }

    /// EvalVectorFloat32 returns VectorFloat32 representation of Constant.
    // go: Constant.EvalVectorFloat32 L1
    pub fn eval_vector_float32(
        &self,
        ctx: &dyn EvalContext,
        st: &EvalState,
        row: Row<'_>,
    ) -> Result<Option<VectorFloat32<'static>>, Error> {
        let dt = self.datum_for_eval(ctx, st, row)?;
        if self.ty(ctx).get_type() == mysql::TypeNull || dt.is_null() {
            return Ok(None);
        }
        Ok(Some(dt.get_vector_float32().clone()))
    }

    /// Equal implements Expression interface.
    // go: Constant.Equal L1
    pub fn equal(&self, ctx: &dyn EvalContext, b: &Expr) -> bool {
        let Expr::Constant(y) = b else {
            return false;
        };
        let st = EvalState::default();
        let err1 = y.eval(ctx, &st, Row::default());
        let err2 = self.eval(ctx, &st, Row::default());
        if err1.is_err() || err2.is_err() {
            return false;
        }
        let (con, r) = self
            .value
            .compare(&type_ctx(ctx), &y.value, get_binary_collator());
        if r.is_err() || con != 0 {
            return false;
        }
        true
    }

    /// IsCorrelated implements Expression interface.
    // go: Constant.IsCorrelated L1
    pub fn is_correlated(&self) -> bool {
        false
    }

    /// ConstLevel returns the const level for the expression
    // go: Constant.ConstLevel L1
    pub fn const_level(&self) -> ConstLevel {
        if self.deferred_expr.is_some() || self.param_marker.is_some() {
            return ConstOnlyInContext;
        }
        ConstStrict
    }

    /// Decorrelate implements Expression interface.
    // go: Constant.Decorrelate L1
    pub fn decorrelate(&self, this: &ExprRef, _schema: &Schema) -> ExprRef {
        Arc::clone(this)
    }

    /// HashCode implements Expression interface.
    // go: Constant.HashCode L1
    pub fn hash_code(&self) -> &[u8] {
        self.get_hash_code(false)
    }

    /// CanonicalHashCode implements Expression interface.
    // go: Constant.CanonicalHashCode L1
    pub fn canonical_hash_code(&self) -> &[u8] {
        self.get_hash_code(true)
    }

    // go: Constant.getHashCode L1
    fn get_hash_code(&self, canonical: bool) -> &[u8] {
        self.hashcode.get_or_init(|| {
            if let Some(de) = &self.deferred_expr {
                if canonical {
                    return de.canonical_hash_code().to_vec();
                }
                return de.hash_code().to_vec();
            }

            if let Some(pm) = &self.param_marker {
                let mut h = vec![parameterFlag];
                tidb_codec::encode_int(&mut h, pm.order);
                return h;
            }

            debug_assert!(self.deferred_expr.is_none() && self.param_marker.is_none());
            let mut h = vec![constantFlag];
            tidb_codec::hash_code(&mut h, &self.value);
            h
        })
    }

    /// ResolveIndices implements Expression interface.
    // go: Constant.ResolveIndices L1
    pub fn resolve_indices(&self, this: &ExprRef, _schema: &Schema) -> Result<ExprRef, Error> {
        Ok(Arc::clone(this))
    }

    // go: Constant.resolveIndices L1
    pub fn resolve_indices_in_place(&mut self, _schema: &Schema) -> Result<(), Error> {
        Ok(())
    }

    /// ResolveIndicesByVirtualExpr implements Expression interface.
    // go: Constant.ResolveIndicesByVirtualExpr L1
    pub fn resolve_indices_by_virtual_expr(
        &self,
        this: &ExprRef,
        _ctx: &dyn EvalContext,
        _schema: &Schema,
    ) -> (ExprRef, bool) {
        (Arc::clone(this), true)
    }

    // go: Constant.resolveIndicesByVirtualExpr L1
    pub fn resolve_indices_by_virtual_expr_in_place(
        &mut self,
        _ctx: &dyn EvalContext,
        _schema: &Schema,
    ) -> bool {
        true
    }

    /// RemapColumn remaps columns with provided mapping and returns new expression
    // go: Constant.RemapColumn L1
    pub fn remap_column(
        &self,
        this: &ExprRef,
        _m: &HashMap<i64, Arc<Column>>,
    ) -> Result<ExprRef, Error> {
        Ok(Arc::clone(this))
    }

    /// Vectorized returns if this expression supports vectorized evaluation.
    // go: Constant.Vectorized L1
    pub fn vectorized(&self) -> bool {
        if let Some(de) = &self.deferred_expr {
            return de.vectorized();
        }
        true
    }

    /// Coercibility returns the coercibility value which is used to check collations.
    // go: Constant.Coercibility L1
    pub fn coercibility(&self) -> Coercibility {
        if !self.collation_info.has_coercibility() {
            self.collation_info
                .set_coercibility(derive_coercibility_for_constant(self));
        }
        self.collation_info.coercibility()
    }

    /// MemoryUsage return the memory usage of Constant
    // go: Constant.MemoryUsage L2 (unsafe.Sizeof(Constant{}) is Go's size on 64-bit targets, spec §4)
    pub fn memory_usage(&self) -> i64 {
        let hashcode_cap = self.hashcode.get().map_or(0, |h| h.capacity()) as i64;
        let mut sum = emptyConstantSize + self.value.mem_usage() + hashcode_cap;
        sum += self.ret_type.memory_usage();
        sum
    }
}

/// `unsafe.Sizeof(Constant{})` of Go v8.5.8 on amd64/arm64.
#[allow(non_upper_case_globals)]
pub(crate) const emptyConstantSize: i64 = 192;
