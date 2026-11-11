//! go-file: pkg/expression/scalar_function.go
//!
//! `ScalarFunction` (spec §3.1, N4 §3.1): the function name, the result type and the
//! signature ([`BuiltinFunc`]), with Go's cached hash codes.
//!
//! - Nodes are immutable and shared (`Arc<Expr>`, M-EXPR). Go's methods that change a node
//!   in place (`Decorrelate`, `RemapColumn` on the clone, `resolveIndices`) build a new node
//!   with the changed arguments; the caches follow Go's (`Clone` copies them, the in-place
//!   changes clear them where Go calls `CleanHashCode`).
//! - The `VecEvalXxx` methods are [`ScalarFunction::vec_eval`] over evaluated argument
//!   columns ([`crate::eval::RpnProgram`], N4 §3.4).
//! - Not ported here: `NewFunction*`/`newFunctionImpl`/`typeInferForNull` need the function
//!   class table (`funcs`) of block D08.2; `Hash64`/`Equals` are cascades-only (EXC.B);
//!   `ReHashCode`'s branch for `grouping` needs `BuiltinGroupingImplSig` (D08.9) and panics
//!   until then; `wrapEvalAssert` is intest-only (spec §4).

#![allow(non_upper_case_globals)]

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use tidb_base::Error;
use tidb_base::errors::RedactLogDisable;
use tidb_base::mysql;
use tidb_chunk::{Column as ChunkColumn, Row};
use tidb_codec::bytes::encode_compact_bytes;
use tidb_parser::model::CIStr;
use tidb_parser::types::{
    ETDatetime, ETDecimal, ETDuration, ETInt, ETJson, ETReal, ETString, ETTimestamp,
    ETVectorFloat32, EvalType,
};
use tidb_types::datum::{Datum, GoValue};
use tidb_types::enum_type::parse_enum;
use tidb_types::etc::FieldType;
use tidb_types::json::BinaryJSON;
use tidb_types::mydecimal::MyDecimal;
use tidb_types::time::{Duration, Time};
use tidb_types::vector::VectorFloat32;

use crate::collation::{Coercibility, Repertoire};
use crate::column::Column;
use crate::context::{EvalContext, ParamValues, type_ctx};
use crate::eval::BuiltinFunc;
use crate::expression::{
    ConstLevel, ConstNone, ConstStrict, EvalState, Expr, ExprRef, TraverseAction,
    scalarFunctionFlag,
};
use crate::schema::Schema;

/// Function names of `pkg/parser/ast/functions.go`.
#[allow(non_upper_case_globals)]
pub mod ast_func {
    /// `ast.LogicAnd`.
    pub const LogicAnd: &str = "and";
    /// `ast.Cast`.
    pub const Cast: &str = "cast";
    /// `ast.LogicOr`.
    pub const LogicOr: &str = "or";
    /// `ast.GE`.
    pub const GE: &str = "ge";
    /// `ast.LE`.
    pub const LE: &str = "le";
    /// `ast.EQ`.
    pub const EQ: &str = "eq";
    /// `ast.LT`.
    pub const LT: &str = "lt";
    /// `ast.GT`.
    pub const GT: &str = "gt";
    /// `ast.Plus`.
    pub const Plus: &str = "plus";
    /// `ast.Minus`.
    pub const Minus: &str = "minus";
    /// `ast.Mul`.
    pub const Mul: &str = "mul";
    /// `ast.UnaryNot`.
    pub const UnaryNot: &str = "not";
    /// `ast.UnaryMinus`.
    pub const UnaryMinus: &str = "unaryminus";
    /// `ast.In`.
    pub const In: &str = "in";
    /// `ast.RowFunc`.
    pub const RowFunc: &str = "row";
    /// `ast.SetVar`.
    pub const SetVar: &str = "setvar";
    /// `ast.GetVar`.
    pub const GetVar: &str = "getvar";
    /// `ast.Values`.
    pub const Values: &str = "values";
    /// `ast.GetParam`.
    pub const GetParam: &str = "getparam";
    /// `ast.Rand`.
    pub const Rand: &str = "rand";
    /// `ast.DayName`.
    pub const DayName: &str = "dayname";
    /// `ast.Sysdate`.
    pub const Sysdate: &str = "sysdate";
    /// `ast.Benchmark`.
    pub const Benchmark: &str = "benchmark";
    /// `ast.FoundRows`.
    pub const FoundRows: &str = "found_rows";
    /// `ast.AnyValue`.
    pub const AnyValue: &str = "any_value";
    /// `ast.Sleep`.
    pub const Sleep: &str = "sleep";
    /// `ast.UUID`.
    pub const UUID: &str = "uuid";
    /// `ast.Grouping`.
    pub const Grouping: &str = "grouping";
    /// `ast.NextVal`.
    pub const NextVal: &str = "nextval";
    /// `ast.LastVal`.
    pub const LastVal: &str = "lastval";
    /// `ast.SetVal`.
    pub const SetVal: &str = "setval";
    /// `ast.TiDBShard`.
    pub const TiDBShard: &str = "tidb_shard";
}

/// unFoldableFunctions stores functions which can not be folded duration constant folding stage.
// go: unFoldableFunctions (function_traits.go var)
pub(crate) const unFoldableFunctions: &[&str] = &[
    ast_func::Sysdate,
    ast_func::FoundRows,
    ast_func::Rand,
    ast_func::UUID,
    ast_func::Sleep,
    ast_func::RowFunc,
    ast_func::Values,
    ast_func::SetVar,
    ast_func::GetVar,
    ast_func::GetParam,
    ast_func::Benchmark,
    ast_func::DayName,
    ast_func::NextVal,
    ast_func::LastVal,
    ast_func::SetVal,
    ast_func::AnyValue,
];

/// `unsafe.Sizeof(ScalarFunction{})` on 64-bit targets (spec §4).
pub(crate) const emptyScalarFunctionSize: i64 = 104;

/// ScalarFunction is the function that returns a value.
#[derive(Debug)]
pub struct ScalarFunction {
    /// FuncName.
    pub func_name: CIStr,
    /// RetType is the type that ScalarFunction returns.
    pub ret_type: FieldType,
    /// Function: the signature and the `baseBuiltinFunc` fields (N4 §3.1).
    pub function: BuiltinFunc,
    /// The cached `HashCode`.
    pub hashcode: OnceLock<Vec<u8>>,
    /// The cached `CanonicalHashCode`.
    pub canonicalhashcode: OnceLock<Vec<u8>>,
}

impl Clone for ScalarFunction {
    /// Clone implements Expression interface.
    // go: ScalarFunction.Clone L2 (the signature's Clone is BuiltinFunc::clone_from_base; collationInfo travels with it)
    fn clone(&self) -> Self {
        let c = ScalarFunction {
            func_name: self.func_name.clone(),
            ret_type: self.ret_type.clone(),
            function: self.function.clone_from_base(),
            hashcode: self.hashcode.clone(),
            canonicalhashcode: self.canonicalhashcode.clone(),
        };
        // c.SetCharsetAndCollation / SetCoercibility / SetRepertoire: the collationInfo is
        // part of the signature and was copied with it (Coercibility() is not called here:
        // Go calls it, which derives it when unset — a panic for a scalar function, see
        // `coercibility`; signatures built by D08.2 always set it).
        c
    }
}

impl ScalarFunction {
    /// A function node over `function` (the struct literal Go's `newFunctionImpl` builds).
    pub fn new(func_name: &str, ret_type: FieldType, function: BuiltinFunc) -> ScalarFunction {
        ScalarFunction {
            func_name: CIStr::new(func_name),
            ret_type,
            function,
            hashcode: OnceLock::new(),
            canonicalhashcode: OnceLock::new(),
        }
    }

    /// GetArgs gets arguments of function.
    // go: ScalarFunction.GetArgs L1
    #[inline]
    pub fn get_args(&self) -> &[ExprRef] {
        self.function.get_args()
    }

    /// Vectorized returns if this expression supports vectorized evaluation.
    // go: ScalarFunction.Vectorized L1
    pub fn vectorized(&self) -> bool {
        self.function.vectorized() && self.function.is_children_vectorized()
    }

    /// StringWithCtx implements Expression interface.
    // go: ScalarFunction.StringWithCtx L1
    pub fn string_with_ctx(&self, ctx: &dyn ParamValues, redact: &str) -> Vec<u8> {
        let mut buffer =
            Vec::with_capacity(self.func_name.l.len() + 8 + 16 * self.get_args().len());
        buffer.extend_from_slice(self.func_name.l.as_bytes());
        buffer.push(b'(');
        if self.func_name.l == ast_func::Cast {
            for arg in self.get_args() {
                buffer.extend_from_slice(&arg.string_with_ctx(ctx, redact));
                buffer.extend_from_slice(b", ");
                buffer.extend_from_slice(self.ret_type.to_string().as_bytes());
            }
        } else {
            let args = self.get_args();
            for (i, arg) in args.iter().enumerate() {
                buffer.extend_from_slice(&arg.string_with_ctx(ctx, redact));
                if i + 1 != args.len() {
                    buffer.extend_from_slice(b", ");
                }
            }
        }
        buffer.push(b')');
        buffer
    }

    /// String implements fmt.Stringer interface.
    // go: ScalarFunction.String L1
    pub fn to_string(&self) -> Vec<u8> {
        self.string_with_ctx(&**tidb_sessionctx::exprctx::EmptyParamValues, RedactLogDisable)
    }

    /// GetType implements Expression interface.
    // go: ScalarFunction.GetType L1
    #[inline]
    pub fn get_type(&self) -> &FieldType {
        self.get_static_type()
    }

    /// GetStaticType implements Expression interface.
    // go: ScalarFunction.GetStaticType L1
    #[inline]
    pub fn get_static_type(&self) -> &FieldType {
        &self.ret_type
    }

    /// Equal implements Expression interface.
    // go: ScalarFunction.Equal L1
    pub fn equal(&self, ctx: &dyn EvalContext, e: &Expr) -> bool {
        let Expr::ScalarFunction(fun) = e else {
            return false;
        };
        if self.func_name.l != fun.func_name.l {
            return false;
        }
        if !self.ret_type.equal(&fun.ret_type) {
            return false;
        }
        self.function.equal(ctx, &fun.function)
    }

    /// IsCorrelated implements Expression interface.
    // go: ScalarFunction.IsCorrelated L1
    pub fn is_correlated(&self) -> bool {
        self.get_args().iter().any(|arg| arg.is_correlated())
    }

    /// ConstLevel returns the const level for the expression. `extensionFuncSig` (extension
    /// functions, not ported) is never a signature here.
    // go: ScalarFunction.ConstLevel L1
    pub fn const_level(&self) -> ConstLevel {
        if unFoldableFunctions.contains(&self.func_name.l.as_str()) {
            return ConstNone;
        }
        let mut level = ConstStrict;
        for arg in self.get_args() {
            let arg_level = arg.const_level();
            if arg_level == ConstNone {
                return ConstNone;
            }
            if arg_level < level {
                level = arg_level;
            }
        }
        level
    }

    /// Decorrelate implements Expression interface. Go rewrites the arguments of this node
    /// and clears its hash codes; here the result is a new node with the same caches cleared.
    // go: ScalarFunction.Decorrelate L2 (new node instead of in place, M-EXPR)
    pub fn decorrelate(&self, _this: &ExprRef, schema: &Schema) -> ExprRef {
        let mut sf = self.with_args(self.get_args().iter().map(|arg| arg.decorrelate(schema)).collect());
        sf.clean_hash_code();
        Expr::ScalarFunction(sf).into_ref()
    }

    /// Traverse implements the TraverseDown interface.
    // go: ScalarFunction.Traverse L1
    pub fn traverse(&self, this: &ExprRef, action: &mut dyn TraverseAction) -> ExprRef {
        action.transform(this)
    }

    /// Eval implements Expression interface.
    // go: ScalarFunction.Eval L1
    pub fn eval(&self, ctx: &dyn EvalContext, st: &EvalState, row: Row<'_>) -> Result<Datum, Error> {
        let tp = self.get_type();
        let res: Option<GoValue> = match tp.eval_type() {
            ETInt => self.eval_int(ctx, st, row)?.map(|v| {
                if mysql::has_unsigned_flag(tp.get_flag()) {
                    GoValue::Uint64(v as u64)
                } else {
                    GoValue::Int64(v)
                }
            }),
            ETReal => self.eval_real(ctx, st, row)?.map(GoValue::Float64),
            ETDecimal => self.eval_decimal(ctx, st, row)?.map(|d| GoValue::Decimal(Box::new(d))),
            ETDatetime | ETTimestamp => self.eval_time(ctx, st, row)?.map(GoValue::Time),
            ETDuration => self.eval_duration(ctx, st, row)?.map(GoValue::Duration),
            ETJson => self.eval_json(ctx, st, row)?.map(GoValue::BinaryJSON),
            ETVectorFloat32 => self.eval_vector_float32(ctx, st, row)?.map(GoValue::VectorFloat32),
            ETString => match self.eval_string(ctx, st, row)? {
                Some(s) if tp.get_type() == mysql::TypeEnum => {
                    let elems = tp.get_elems().unwrap_or(&[]);
                    match parse_enum(elems, &s, tp.get_collate()) {
                        Ok(e) => Some(GoValue::Enum(e)),
                        Err(err) => {
                            // res is the zero Enum Go's ParseEnum returns with the error.
                            type_ctx(ctx).handle_truncate(Err(err))?;
                            Some(GoValue::Enum(Default::default()))
                        }
                    }
                }
                Some(s) => Some(GoValue::String(s)),
                None => None,
            },
            _ => None,
        };
        let mut d = Datum::default();
        match res {
            None => d.set_null(),
            Some(v) => d.set_value(v, &self.ret_type),
        }
        Ok(d)
    }

    /// EvalInt implements Expression interface.
    // go: ScalarFunction.EvalInt L1
    pub fn eval_int(&self, ctx: &dyn EvalContext, st: &EvalState, row: Row<'_>) -> Result<Option<i64>, Error> {
        self.function.eval_int(ctx, st, row)
    }

    /// EvalReal implements Expression interface.
    // go: ScalarFunction.EvalReal L1
    pub fn eval_real(&self, ctx: &dyn EvalContext, st: &EvalState, row: Row<'_>) -> Result<Option<f64>, Error> {
        self.function.eval_real(ctx, st, row)
    }

    /// EvalDecimal implements Expression interface.
    // go: ScalarFunction.EvalDecimal L1
    pub fn eval_decimal(
        &self,
        ctx: &dyn EvalContext,
        st: &EvalState,
        row: Row<'_>,
    ) -> Result<Option<MyDecimal>, Error> {
        self.function.eval_decimal(ctx, st, row)
    }

    /// EvalString implements Expression interface.
    // go: ScalarFunction.EvalString L1
    pub fn eval_string(
        &self,
        ctx: &dyn EvalContext,
        st: &EvalState,
        row: Row<'_>,
    ) -> Result<Option<Vec<u8>>, Error> {
        self.function.eval_string(ctx, st, row)
    }

    /// EvalTime implements Expression interface.
    // go: ScalarFunction.EvalTime L1
    pub fn eval_time(&self, ctx: &dyn EvalContext, st: &EvalState, row: Row<'_>) -> Result<Option<Time>, Error> {
        self.function.eval_time(ctx, st, row)
    }

    /// EvalDuration implements Expression interface.
    // go: ScalarFunction.EvalDuration L1
    pub fn eval_duration(
        &self,
        ctx: &dyn EvalContext,
        st: &EvalState,
        row: Row<'_>,
    ) -> Result<Option<Duration>, Error> {
        self.function.eval_duration(ctx, st, row)
    }

    /// EvalJSON implements Expression interface.
    // go: ScalarFunction.EvalJSON L1
    pub fn eval_json(
        &self,
        ctx: &dyn EvalContext,
        st: &EvalState,
        row: Row<'_>,
    ) -> Result<Option<BinaryJSON<'static>>, Error> {
        self.function.eval_json(ctx, st, row)
    }

    /// EvalVectorFloat32 implements Expression interface.
    // go: ScalarFunction.EvalVectorFloat32 L1
    pub fn eval_vector_float32(
        &self,
        ctx: &dyn EvalContext,
        st: &EvalState,
        row: Row<'_>,
    ) -> Result<Option<VectorFloat32<'static>>, Error> {
        self.function.eval_vector_float32(ctx, st, row)
    }

    /// `VecEvalInt` … `VecEvalVectorFloat32`: the vector driver of the signature for the
    /// evaluation type `et`, over `n` rows of evaluated argument columns.
    // go: ScalarFunction.VecEvalInt L2 (one entry for the eight VecEvalXxx; arguments are evaluated by the RPN program, N4 §3.4)
    // go: ScalarFunction.VecEvalReal L2 (see VecEvalInt)
    // go: ScalarFunction.VecEvalDecimal L2 (see VecEvalInt)
    // go: ScalarFunction.VecEvalString L2 (see VecEvalInt)
    // go: ScalarFunction.VecEvalTime L2 (see VecEvalInt)
    // go: ScalarFunction.VecEvalDuration L2 (see VecEvalInt)
    // go: ScalarFunction.VecEvalJSON L2 (see VecEvalInt)
    // go: ScalarFunction.VecEvalVectorFloat32 L2 (see VecEvalInt)
    pub fn vec_eval(
        &self,
        ctx: &dyn EvalContext,
        et: EvalType,
        n: usize,
        cols: &[&ChunkColumn],
        result: &mut ChunkColumn,
    ) -> Result<(), Error> {
        self.function.vec_eval(ctx, et, n, cols, result)
    }

    /// HashCode implements Expression interface.
    // go: ScalarFunction.HashCode L1
    pub fn hash_code(&self) -> &[u8] {
        self.hashcode.get_or_init(|| re_hash_code(self))
    }

    /// CanonicalHashCode implements Expression interface.
    // go: ScalarFunction.CanonicalHashCode L1
    pub fn canonical_hash_code(&self) -> &[u8] {
        self.canonicalhashcode.get_or_init(|| simple_canonicalized_hash_code(self))
    }

    /// CleanHashCode will clean the hashcode you may be cached before. It's used especially in
    /// CSE module, where we will substitute the expression with a new one.
    // go: ScalarFunction.CleanHashCode L1
    pub fn clean_hash_code(&mut self) {
        self.hashcode = OnceLock::new();
        self.canonicalhashcode = OnceLock::new();
    }

    /// ResolveIndices implements Expression interface.
    // go: ScalarFunction.ResolveIndices L1
    pub fn resolve_indices(&self, _this: &ExprRef, schema: &Schema) -> Result<ExprRef, Error> {
        let mut new_sf = self.clone();
        new_sf.resolve_indices_in_place(schema)?;
        Ok(Expr::ScalarFunction(new_sf).into_ref())
    }

    /// The hash codes are kept, as in Go (they do not depend on the indices).
    // go: ScalarFunction.resolveIndices L2 (arguments replaced by resolved copies, M-EXPR)
    pub fn resolve_indices_in_place(&mut self, schema: &Schema) -> Result<(), Error> {
        let mut args = Vec::with_capacity(self.get_args().len());
        for arg in self.get_args() {
            args.push(arg.resolve_indices(schema)?);
        }
        self.function.args = args;
        Ok(())
    }

    /// ResolveIndicesByVirtualExpr implements Expression interface.
    // go: ScalarFunction.ResolveIndicesByVirtualExpr L1
    pub fn resolve_indices_by_virtual_expr(
        &self,
        _this: &ExprRef,
        ctx: &dyn EvalContext,
        schema: &Schema,
    ) -> (ExprRef, bool) {
        let mut new_sf = self.clone();
        let is_ok = new_sf.resolve_indices_by_virtual_expr_in_place(ctx, schema);
        (Expr::ScalarFunction(new_sf).into_ref(), is_ok)
    }

    /// Go stops at the first argument that fails and leaves the later ones as they were.
    // go: ScalarFunction.resolveIndicesByVirtualExpr L2 (arguments replaced by resolved copies, M-EXPR)
    pub fn resolve_indices_by_virtual_expr_in_place(
        &mut self,
        ctx: &dyn EvalContext,
        schema: &Schema,
    ) -> bool {
        let mut args = self.get_args().to_vec();
        let mut ok = true;
        for arg in args.iter_mut() {
            let (new_arg, is_ok) = arg.resolve_indices_by_virtual_expr(ctx, schema);
            *arg = new_arg;
            if !is_ok {
                ok = false;
                break;
            }
        }
        self.function.args = args;
        ok
    }

    /// RemapColumn remaps columns with provided mapping and returns new expression
    // go: ScalarFunction.RemapColumn L1
    pub fn remap_column(&self, _this: &ExprRef, m: &HashMap<i64, Arc<Column>>) -> Result<ExprRef, Error> {
        let mut new_args = Vec::with_capacity(self.get_args().len());
        for arg in self.get_args() {
            new_args.push(arg.remap_column(m)?);
        }
        let mut new_sf = self.with_args(new_args);
        new_sf.clean_hash_code();
        Ok(Expr::ScalarFunction(new_sf).into_ref())
    }

    /// GetSingleColumn returns (Col, Desc) when the ScalarFunction is equivalent to (Col, Desc)
    /// when used as a sort key and window frame.
    // go: ScalarFunction.GetSingleColumn L1
    pub fn get_single_column(&self, reverse: bool) -> (Option<&Column>, bool) {
        let args = self.get_args();
        match self.func_name.o.as_str() {
            ast_func::Plus => match &*args[0] {
                Expr::Column(tp) => {
                    if !matches!(&*args[1], Expr::Constant(_)) {
                        return (None, false);
                    }
                    (Some(tp), reverse)
                }
                Expr::ScalarFunction(tp) => {
                    if !matches!(&*args[1], Expr::Constant(_)) {
                        return (None, false);
                    }
                    tp.get_single_column(reverse)
                }
                Expr::Constant(_) => match &*args[1] {
                    Expr::Column(rtp) => (Some(rtp), reverse),
                    Expr::ScalarFunction(rtp) => rtp.get_single_column(reverse),
                    _ => (None, false),
                },
                _ => (None, false),
            },
            ast_func::Minus => match &*args[0] {
                Expr::Column(tp) => {
                    if !matches!(&*args[1], Expr::Constant(_)) {
                        return (None, false);
                    }
                    (Some(tp), reverse)
                }
                Expr::ScalarFunction(tp) => {
                    if !matches!(&*args[1], Expr::Constant(_)) {
                        return (None, false);
                    }
                    tp.get_single_column(reverse)
                }
                Expr::Constant(_) => match &*args[1] {
                    Expr::Column(rtp) => (Some(rtp), !reverse),
                    Expr::ScalarFunction(rtp) => rtp.get_single_column(!reverse),
                    _ => (None, false),
                },
                _ => (None, false),
            },
            ast_func::UnaryMinus => match &*args[0] {
                Expr::Column(tp) => (Some(tp), !reverse),
                Expr::ScalarFunction(tp) => tp.get_single_column(!reverse),
                _ => (None, false),
            },
            _ => (None, false),
        }
    }

    /// Coercibility returns the coercibility value which is used to check collations.
    /// Go derives it with `deriveCoercibilityForScalarFunc`, which panics: every signature
    /// sets it when it is built.
    // go: ScalarFunction.Coercibility L1
    pub fn coercibility(&self) -> Coercibility {
        if !self.function.collation_info.has_coercibility() {
            self.set_coercibility(derive_coercibility_for_scalar_func(self));
        }
        self.function.collation_info.coercibility()
    }

    /// HasCoercibility ...
    // go: ScalarFunction.HasCoercibility L1
    pub fn has_coercibility(&self) -> bool {
        self.function.collation_info.has_coercibility()
    }

    /// SetCoercibility sets a specified coercibility for this expression.
    // go: ScalarFunction.SetCoercibility L1
    pub fn set_coercibility(&self, val: Coercibility) {
        self.function.collation_info.set_coercibility(val);
    }

    /// CharsetAndCollation gets charset and collation.
    // go: ScalarFunction.CharsetAndCollation L1
    pub fn charset_and_collation(&self) -> (&str, &str) {
        self.function.collation_info.charset_and_collation()
    }

    /// SetCharsetAndCollation sets charset and collation.
    // go: ScalarFunction.SetCharsetAndCollation L1
    pub fn set_charset_and_collation(&mut self, chs: &str, coll: &str) {
        self.function.collation_info.set_charset_and_collation(chs, coll);
    }

    /// Repertoire returns the repertoire value which is used to check collations.
    // go: ScalarFunction.Repertoire L1
    pub fn repertoire(&self) -> Repertoire {
        self.function.collation_info.repertoire()
    }

    /// SetRepertoire sets a specified repertoire for this expression.
    // go: ScalarFunction.SetRepertoire L1
    pub fn set_repertoire(&mut self, r: Repertoire) {
        self.function.collation_info.set_repertoire(r);
    }

    /// IsExplicitCharset return the charset is explicit set or not.
    // go: ScalarFunction.IsExplicitCharset L1
    pub fn is_explicit_charset(&self) -> bool {
        self.function.collation_info.is_explicit_charset()
    }

    /// SetExplicitCharset set the charset is explicit or not.
    // go: ScalarFunction.SetExplicitCharset L1
    pub fn set_explicit_charset(&mut self, explicit: bool) {
        self.function.collation_info.set_explicit_charset(explicit);
    }

    /// MemoryUsage return the memory usage of ScalarFunction. `cap(sf.hashcode)` is the
    /// length here (spec §4: Go's capacity follows `append` growth).
    // go: ScalarFunction.MemoryUsage L2 (hash code capacity, spec §4)
    pub fn memory_usage(&self) -> i64 {
        let mut sum = emptyScalarFunctionSize
            + (self.func_name.l.len() + self.func_name.o.len()) as i64
            + self.hashcode.get().map_or(0, |h| h.len() as i64);
        sum += self.ret_type.memory_usage();
        sum += self.function.memory_usage();
        sum
    }

    /// The node with other arguments and the same everything else (caches included, as a
    /// Go clone whose arguments are then replaced).
    pub(crate) fn with_args(&self, args: Vec<ExprRef>) -> ScalarFunction {
        let mut sf = self.clone();
        sf.function.args = args;
        sf
    }
}

/// deriveCoercibilityForScalarFunc: never called in Go.
// go: deriveCoercibilityForScalarFunc L1
pub(crate) fn derive_coercibility_for_scalar_func(_sf: &ScalarFunction) -> Coercibility {
    panic!("this function should never be called")
}

/// ScalarFuncs2Exprs converts []*ScalarFunction to []Expression.
// go: ScalarFuncs2Exprs L1
pub fn scalar_funcs_2_exprs(funcs: &[Arc<Expr>]) -> Vec<ExprRef> {
    let mut result = Vec::with_capacity(funcs.len());
    for col in funcs {
        result.push(Arc::clone(col));
    }
    result
}

/// ExpressionsSemanticEqual is used to judge whether two expression tree is semantic
/// equivalent.
// go: ExpressionsSemanticEqual L1
pub fn expressions_semantic_equal(expr1: &Expr, expr2: &Expr) -> bool {
    expr1.canonical_hash_code() == expr2.canonical_hash_code()
}

/// simpleCanonicalizedHashCode is used to judge whether two expression is semantically
/// equal under simple predicate (commutative arguments sorted, `a >= b` = `b <= a`, `NOT`
/// pushed into a comparison).
// go: simpleCanonicalizedHashCode L1
fn simple_canonicalized_hash_code(sf: &ScalarFunction) -> Vec<u8> {
    let mut code = vec![scalarFunctionFlag];
    let mut args_hash_code: Vec<&[u8]> = sf.get_args().iter().map(|arg| arg.canonical_hash_code()).collect();
    let name = sf.func_name.l.as_str();
    match name {
        ast_func::Plus | ast_func::Mul | ast_func::EQ | ast_func::In | ast_func::LogicOr | ast_func::LogicAnd => {
            encode_compact_bytes(&mut code, name.as_bytes());
            args_hash_code.sort();
            for arg_code in args_hash_code {
                code.extend_from_slice(arg_code);
            }
        }
        // directed binary OP: a >= b and b <= a should have the same hashcode.
        ast_func::GE | ast_func::LE => {
            encode_compact_bytes(&mut code, ast_func::GE.as_bytes());
            if name == ast_func::GE {
                for arg_code in &args_hash_code {
                    code.extend_from_slice(arg_code);
                }
            } else {
                for arg_code in args_hash_code.iter().rev() {
                    code.extend_from_slice(arg_code);
                }
            }
        }
        ast_func::GT | ast_func::LT => {
            encode_compact_bytes(&mut code, ast_func::GT.as_bytes());
            if name == ast_func::GT {
                for arg_code in &args_hash_code {
                    code.extend_from_slice(arg_code);
                }
            } else {
                for arg_code in args_hash_code.iter().rev() {
                    code.extend_from_slice(arg_code);
                }
            }
        }
        ast_func::UnaryNot => match &*sf.get_args()[0] {
            Expr::ScalarFunction(child) => {
                let child_args_hash_code: Vec<&[u8]> =
                    child.get_args().iter().map(|arg| arg.canonical_hash_code()).collect();
                match child.func_name.l.as_str() {
                    // not GT  ==> LE  ==> use GE and switch args
                    ast_func::GT => {
                        encode_compact_bytes(&mut code, ast_func::GE.as_bytes());
                        for arg_code in child_args_hash_code.iter().rev() {
                            code.extend_from_slice(arg_code);
                        }
                    }
                    // not LT  ==> GE
                    ast_func::LT => {
                        encode_compact_bytes(&mut code, ast_func::GE.as_bytes());
                        for arg_code in &child_args_hash_code {
                            code.extend_from_slice(arg_code);
                        }
                    }
                    // not GE  ==> LT  ==> use GT and switch args
                    ast_func::GE => {
                        encode_compact_bytes(&mut code, ast_func::GT.as_bytes());
                        for arg_code in child_args_hash_code.iter().rev() {
                            code.extend_from_slice(arg_code);
                        }
                    }
                    // not LE  ==> GT
                    ast_func::LE => {
                        encode_compact_bytes(&mut code, ast_func::GT.as_bytes());
                        for arg_code in &child_args_hash_code {
                            code.extend_from_slice(arg_code);
                        }
                    }
                    // Go writes nothing more for NOT over another function.
                    _ => {}
                }
            }
            _ => {
                encode_compact_bytes(&mut code, name.as_bytes());
                for arg_code in &args_hash_code {
                    code.extend_from_slice(arg_code);
                }
            }
        },
        _ => {
            encode_compact_bytes(&mut code, name.as_bytes());
            for arg_code in &args_hash_code {
                code.extend_from_slice(arg_code);
            }
            if name == ast_func::Cast {
                let eval_tp = sf.ret_type.eval_type();
                code.push(eval_tp.0);
            }
        }
    }
    code
}

/// ReHashCode is used after we change the argument in place. Returns the new hash code
/// (Go stores it in `sf.hashcode` and clears `canonicalhashcode`; [`ScalarFunction::hash_code`]
/// stores it, [`ScalarFunction::clean_hash_code`] clears both).
// go: ReHashCode L2 (returns the code; grouping branch waits for BuiltinGroupingImplSig, D08.9)
pub fn re_hash_code(sf: &ScalarFunction) -> Vec<u8> {
    let mut code = vec![scalarFunctionFlag];
    encode_compact_bytes(&mut code, sf.func_name.l.as_bytes());
    for arg in sf.get_args() {
        code.extend_from_slice(arg.hash_code());
    }
    if sf.func_name.l == ast_func::Cast {
        let eval_tp = sf.ret_type.eval_type();
        code.push(eval_tp.0);
    }
    if sf.func_name.l == ast_func::Grouping {
        // Registered exemption: BuiltinGroupingImplSig (grouping mode and marks) is D08.9.
        panic!("ReHashCode of grouping needs BuiltinGroupingImplSig (D08.9)");
    }
    code
}
