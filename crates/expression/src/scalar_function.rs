//! `ScalarFunction` in its transitional form (spec §3.3).
//!
//! Part 1 needs the variant to exist so that `match` arms and argument walks
//! (`ExprReferenceSchema`, `IsEQCondFromIn`, `GcColumnExprIsTidbShard`) are written as in
//! Go. The struct keeps the fields those walks read; evaluation, `Equal`, `String`,
//! `HashCode` and the other interface methods are temporary stubs (constitution article
//! 4.3, class "temporary") that panic until part 2 ports `scalar_function.go` and the
//! `BuiltinFunc`/`SigDesc` framework of N4 §3.1–3.4. No Go function of
//! `scalar_function.go` is claimed here: converge counts the file as not started.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;

use tidb_base::Error;
use tidb_chunk::Row;
use tidb_parser::model::CIStr;
use tidb_types::datum::Datum;
use tidb_types::etc::FieldType;
use tidb_types::json::BinaryJSON;
use tidb_types::mydecimal::MyDecimal;
use tidb_types::time::{Duration, Time};
use tidb_types::vector::VectorFloat32;

use crate::collation::{Coercibility, Repertoire};
use crate::column::Column;
use crate::context::{EvalContext, ParamValues};
use crate::expression::{ConstLevel, EvalState, Expr, ExprRef, TraverseAction};
use crate::schema::Schema;

/// Function names of `pkg/parser/ast/functions.go` used by part 1.
#[allow(non_upper_case_globals)]
pub mod ast_func {
    /// `ast.EQ`.
    pub const EQ: &str = "eq";
    /// `ast.TiDBShard`.
    pub const TiDBShard: &str = "tidb_shard";
}

/// ScalarFunction is the function that returns a value (transitional form, spec §3.3).
#[derive(Clone, Debug)]
pub struct ScalarFunction {
    /// FuncName.
    pub func_name: CIStr,
    /// RetType is the type that ScalarFunction returns.
    pub ret_type: FieldType,
    /// The arguments (`Function.getArgs()` in Go).
    pub args: Vec<ExprRef>,
}

macro_rules! part2_stub {
    ($what:literal) => {
        panic!(concat!(
            "ScalarFunction.",
            $what,
            " is not ported yet (D08.1 part 2)"
        ))
    };
}

impl ScalarFunction {
    /// GetArgs gets arguments of function.
    // glue: transitional ScalarFunction (spec §3.3); ScalarFunction.GetArgs is ported with part 2
    pub fn get_args(&self) -> &[ExprRef] {
        &self.args
    }

    /// The function's return type.
    // glue: transitional ScalarFunction (spec §3.3)
    pub fn get_type(&self) -> &FieldType {
        &self.ret_type
    }

    // glue: temporary stub until D08.1 part 2
    pub(crate) fn traverse(&self, _this: &ExprRef, _action: &mut dyn TraverseAction) -> ExprRef {
        part2_stub!("Traverse")
    }

    // glue: temporary stub until D08.1 part 2
    pub(crate) fn eval(
        &self,
        _ctx: &dyn EvalContext,
        _st: &EvalState,
        _row: Row<'_>,
    ) -> Result<Datum, Error> {
        part2_stub!("Eval")
    }

    // glue: temporary stub until D08.1 part 2
    pub(crate) fn eval_int(
        &self,
        _ctx: &dyn EvalContext,
        _st: &EvalState,
        _row: Row<'_>,
    ) -> Result<Option<i64>, Error> {
        part2_stub!("EvalInt")
    }

    // glue: temporary stub until D08.1 part 2
    pub(crate) fn eval_real(
        &self,
        _ctx: &dyn EvalContext,
        _st: &EvalState,
        _row: Row<'_>,
    ) -> Result<Option<f64>, Error> {
        part2_stub!("EvalReal")
    }

    // glue: temporary stub until D08.1 part 2
    pub(crate) fn eval_string(
        &self,
        _ctx: &dyn EvalContext,
        _st: &EvalState,
        _row: Row<'_>,
    ) -> Result<Option<Vec<u8>>, Error> {
        part2_stub!("EvalString")
    }

    // glue: temporary stub until D08.1 part 2
    pub(crate) fn eval_decimal(
        &self,
        _ctx: &dyn EvalContext,
        _st: &EvalState,
        _row: Row<'_>,
    ) -> Result<Option<MyDecimal>, Error> {
        part2_stub!("EvalDecimal")
    }

    // glue: temporary stub until D08.1 part 2
    pub(crate) fn eval_time(
        &self,
        _ctx: &dyn EvalContext,
        _st: &EvalState,
        _row: Row<'_>,
    ) -> Result<Option<Time>, Error> {
        part2_stub!("EvalTime")
    }

    // glue: temporary stub until D08.1 part 2
    pub(crate) fn eval_duration(
        &self,
        _ctx: &dyn EvalContext,
        _st: &EvalState,
        _row: Row<'_>,
    ) -> Result<Option<Duration>, Error> {
        part2_stub!("EvalDuration")
    }

    // glue: temporary stub until D08.1 part 2
    pub(crate) fn eval_json(
        &self,
        _ctx: &dyn EvalContext,
        _st: &EvalState,
        _row: Row<'_>,
    ) -> Result<Option<BinaryJSON<'static>>, Error> {
        part2_stub!("EvalJSON")
    }

    // glue: temporary stub until D08.1 part 2
    pub(crate) fn eval_vector_float32(
        &self,
        _ctx: &dyn EvalContext,
        _st: &EvalState,
        _row: Row<'_>,
    ) -> Result<Option<VectorFloat32<'static>>, Error> {
        part2_stub!("EvalVectorFloat32")
    }

    // glue: temporary stub until D08.1 part 2
    pub(crate) fn string_with_ctx(&self, _ctx: &dyn ParamValues, _redact: &str) -> Vec<u8> {
        part2_stub!("StringWithCtx")
    }

    // glue: temporary stub until D08.1 part 2
    pub(crate) fn equal(&self, _ctx: &dyn EvalContext, _e: &Expr) -> bool {
        part2_stub!("Equal")
    }

    // glue: temporary stub until D08.1 part 2
    pub(crate) fn is_correlated(&self) -> bool {
        part2_stub!("IsCorrelated")
    }

    // glue: temporary stub until D08.1 part 2
    pub(crate) fn const_level(&self) -> ConstLevel {
        part2_stub!("ConstLevel")
    }

    // glue: temporary stub until D08.1 part 2
    pub(crate) fn decorrelate(&self, _this: &ExprRef, _schema: &Schema) -> ExprRef {
        part2_stub!("Decorrelate")
    }

    // glue: temporary stub until D08.1 part 2
    pub(crate) fn resolve_indices(
        &self,
        _this: &ExprRef,
        _schema: &Schema,
    ) -> Result<ExprRef, Error> {
        part2_stub!("ResolveIndices")
    }

    // glue: temporary stub until D08.1 part 2
    pub(crate) fn remap_column(
        &self,
        _this: &ExprRef,
        _m: &HashMap<i64, Arc<Column>>,
    ) -> Result<ExprRef, Error> {
        part2_stub!("RemapColumn")
    }

    // glue: temporary stub until D08.1 part 2
    pub(crate) fn hash_code(&self) -> &[u8] {
        part2_stub!("HashCode")
    }

    // glue: temporary stub until D08.1 part 2
    pub(crate) fn canonical_hash_code(&self) -> &[u8] {
        part2_stub!("CanonicalHashCode")
    }

    // glue: temporary stub until D08.1 part 2
    pub(crate) fn vectorized(&self) -> bool {
        part2_stub!("Vectorized")
    }

    // glue: temporary stub until D08.1 part 2
    pub(crate) fn coercibility(&self) -> Coercibility {
        part2_stub!("Coercibility")
    }

    // glue: temporary stub until D08.1 part 2
    pub(crate) fn repertoire(&self) -> Repertoire {
        part2_stub!("Repertoire")
    }

    // glue: temporary stub until D08.1 part 2
    pub(crate) fn memory_usage(&self) -> i64 {
        part2_stub!("MemoryUsage")
    }

    /// Unused helper so `Cow` stays imported for part 2's `GetType(ctx)`.
    // glue: temporary stub until D08.1 part 2
    #[allow(dead_code)]
    pub(crate) fn get_type_ctx(&self, _ctx: &dyn EvalContext) -> Cow<'_, FieldType> {
        Cow::Borrowed(&self.ret_type)
    }
}
