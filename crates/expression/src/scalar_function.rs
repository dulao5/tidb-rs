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
//! - `NewFunction*`/`newFunctionImpl`/`typeInferForNull` build CAST, `to_binary` and
//!   `from_binary`; the classes of `funcs` follow their families (D08.3–D08.9, see
//!   [`crate::builtin_registry::function_class`]). `Hash64`/`Equals` are cascades-only (EXC.B);
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

use tidb_base::dbterror::plannererrors::ErrNoDB;
use tidb_sessionctx::variable::varsutil::{OffInt, OnInt};

use crate::builtin_cast::build_cast_function;
use crate::builtin_convert_charset::{
    InternalFuncFromBinary, InternalFuncToBinary, build_from_binary_function,
    build_to_binary_function,
};
use crate::builtin_registry::{function_class, lookup};
use crate::collation::{Coercibility, Repertoire, derive_coercibility_for_scalar_func};
use crate::column::Column;
use crate::constant_fold::fold_constant;
use crate::context::{BuildContext, truncate_warnings, warning_count};
use crate::context::{EvalContext, ParamValues, type_ctx};
use crate::errors::{ErrFunctionNotExists, ErrFunctionsNoopImpl};
use crate::eval::BuiltinFunc;
use crate::expression::{
    ConstLevel, ConstNone, ConstStrict, EvalState, Expr, ExprRef, TraverseAction,
    scalarFunctionFlag,
};
use crate::function_traits::noopFuncs;
use crate::schema::Schema;

/// Function names of `pkg/parser/ast/functions.go`.
pub use crate::ast_funcs as ast_func;
pub(crate) use crate::function_traits::unFoldableFunctions;

/// `unsafe.Sizeof(ScalarFunction{})` on 64-bit targets (spec §4).
pub(crate) const emptyScalarFunctionSize: i64 = 104;

/// ScalarFunction is the function that returns a value.
#[derive(Debug)]
pub struct ScalarFunction {
    /// FuncName.
    pub func_name: CIStr,
    /// Function: the signature and the `baseBuiltinFunc` fields (N4 §3.1). Go's `RetType`
    /// is the same `*FieldType` as the signature's `tp` (`ScalarFunction.Clone` relies on
    /// it), so the node has no type of its own: [`ScalarFunction::ret_type`] is
    /// `function.tp` (one type, D08.1 spec §3.2; Lucky review #94).
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
    /// `ret_type` becomes the signature's `tp`: every Go literal passes `f.getRetTp()` or the
    /// pointer the signature was built with, so the two are one type.
    // glue: constructor of the node (Go builds ScalarFunction literals in newFunctionImpl)
    pub fn new(
        func_name: &str,
        ret_type: impl Into<Arc<FieldType>>,
        mut function: BuiltinFunc,
    ) -> ScalarFunction {
        function.tp = ret_type.into();
        ScalarFunction {
            func_name: CIStr::new(func_name),
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
                buffer.extend_from_slice(self.ret_type().to_string().as_bytes());
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
        self.string_with_ctx(
            &**tidb_sessionctx::exprctx::EmptyParamValues,
            RedactLogDisable,
        )
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
        &self.function.tp
    }

    /// `RetType`: the signature's `tp` (one type, see [`ScalarFunction::function`]).
    // glue: field access of ScalarFunction.RetType (one *FieldType with the signature's tp)
    #[inline]
    pub fn ret_type(&self) -> &FieldType {
        &self.function.tp
    }

    /// `RetType` for an in-place change (Go's `sf.RetType.SetXxx(..)` / `GetType(..).SetXxx`):
    /// changes the signature's `tp` too, as in Go. Clears the cached hash codes like Go's
    /// callers that change the type do (the hash includes the type).
    // glue: in-place change of the shared *FieldType
    #[inline]
    pub fn ret_type_mut(&mut self) -> &mut FieldType {
        self.hashcode = OnceLock::new();
        self.canonicalhashcode = OnceLock::new();
        Arc::make_mut(&mut self.function.tp)
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
        if !self.ret_type().equal(fun.ret_type()) {
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
        let mut sf = self.with_args(
            self.get_args()
                .iter()
                .map(|arg| arg.decorrelate(schema))
                .collect(),
        );
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
    pub fn eval(
        &self,
        ctx: &dyn EvalContext,
        st: &EvalState,
        row: Row<'_>,
    ) -> Result<Datum, Error> {
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
            ETDecimal => self
                .eval_decimal(ctx, st, row)?
                .map(|d| GoValue::Decimal(Box::new(d))),
            ETDatetime | ETTimestamp => self.eval_time(ctx, st, row)?.map(GoValue::Time),
            ETDuration => self.eval_duration(ctx, st, row)?.map(GoValue::Duration),
            ETJson => self.eval_json(ctx, st, row)?.map(GoValue::BinaryJSON),
            ETVectorFloat32 => self
                .eval_vector_float32(ctx, st, row)?
                .map(GoValue::VectorFloat32),
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
            Some(v) => d.set_value(v, self.ret_type()),
        }
        Ok(d)
    }

    /// EvalInt implements Expression interface.
    // go: ScalarFunction.EvalInt L1
    pub fn eval_int(
        &self,
        ctx: &dyn EvalContext,
        st: &EvalState,
        row: Row<'_>,
    ) -> Result<Option<i64>, Error> {
        self.function.eval_int(ctx, st, row)
    }

    /// EvalReal implements Expression interface.
    // go: ScalarFunction.EvalReal L1
    pub fn eval_real(
        &self,
        ctx: &dyn EvalContext,
        st: &EvalState,
        row: Row<'_>,
    ) -> Result<Option<f64>, Error> {
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
    pub fn eval_time(
        &self,
        ctx: &dyn EvalContext,
        st: &EvalState,
        row: Row<'_>,
    ) -> Result<Option<Time>, Error> {
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
        self.canonicalhashcode
            .get_or_init(|| simple_canonicalized_hash_code(self))
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
    pub fn remap_column(
        &self,
        _this: &ExprRef,
        m: &HashMap<i64, Arc<Column>>,
    ) -> Result<ExprRef, Error> {
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
        self.function
            .collation_info
            .set_charset_and_collation(chs, coll);
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
        // Go adds RetType.MemoryUsage() and the signature's (which counts tp again): one
        // *FieldType counted twice; kept for equal numbers.
        sum += self.ret_type().memory_usage();
        sum += self.function.memory_usage();
        sum
    }

    /// The node with other arguments and the same everything else (caches included, as a
    /// Go clone whose arguments are then replaced).
    // glue: Go clone whose arguments are then replaced
    pub(crate) fn with_args(&self, args: Vec<ExprRef>) -> ScalarFunction {
        let mut sf = self.clone();
        sf.function.args = args;
        sf
    }
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
    let mut args_hash_code: Vec<&[u8]> = sf
        .get_args()
        .iter()
        .map(|arg| arg.canonical_hash_code())
        .collect();
    let name = sf.func_name.l.as_str();
    match name {
        ast_func::Plus
        | ast_func::Mul
        | ast_func::EQ
        | ast_func::In
        | ast_func::LogicOr
        | ast_func::LogicAnd => {
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
                let child_args_hash_code: Vec<&[u8]> = child
                    .get_args()
                    .iter()
                    .map(|arg| arg.canonical_hash_code())
                    .collect();
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
                let eval_tp = sf.ret_type().eval_type();
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
        let eval_tp = sf.ret_type().eval_type();
        code.push(eval_tp.0);
    }
    if sf.func_name.l == ast_func::Grouping {
        // Registered exemption: BuiltinGroupingImplSig (grouping mode and marks) is D08.9.
        panic!("ReHashCode of grouping needs BuiltinGroupingImplSig (D08.9)");
    }
    code
}

/// typeInferForNull infers the NULL constants field type and set the field type of NULL
/// constant same as other non-null operands.
// go: typeInferForNull L1
pub fn type_infer_for_null(ctx: &dyn EvalContext, args: &mut [ExprRef]) {
    if args.len() < 2 {
        return;
    }
    let is_null = |expr: &Expr| -> bool {
        matches!(expr, Expr::Constant(cons)
            if cons.ret_type.get_type() == mysql::TypeNull && cons.value.is_null())
    };
    // Infer the actual field type of the NULL constant.
    let mut ret_field_tp: Option<FieldType> = None;
    let mut has_null_arg = false;
    for i in (0..args.len()).rev() {
        let is_null_arg = is_null(&args[i]);
        if !is_null_arg && ret_field_tp.is_none() {
            ret_field_tp = Some(args[i].get_type(ctx).into_owned());
        }
        has_null_arg = has_null_arg || is_null_arg;
        // Break if there are both NULL and non-NULL expression
        if has_null_arg && ret_field_tp.is_some() {
            break;
        }
    }
    let Some(ret_field_tp) = ret_field_tp else {
        return;
    };
    if !has_null_arg {
        return;
    }
    for arg in args.iter_mut() {
        let argflags = arg.get_type(ctx);
        if is_null(arg)
            && !(argflags.equals(Some(&ret_field_tp))
                && mysql::has_not_null_flag(ret_field_tp.get_flag()))
        {
            // Go writes through `GetType(ctx)`; a parameter marker's `GetType` returns a new
            // object each time, so the change is lost and its RetType stays TypeNull
            // (`with_type_mut` skips parameter markers; review #95).
            crate::builtin_cast::with_type_mut(arg, |tp| {
                *tp = ret_field_tp.clone();
                tp.del_flag(mysql::NotNullFlag); // Remove NotNullFlag of NullConst
            });
        }
    }
}

/// ScalarFunctionCallBack is the definition of callback of calling a newFunction.
pub type ScalarFunctionCallBack = fn(ScalarFunction) -> Result<ExprRef, Error>;

/// newFunctionImpl creates a new scalar function or constant.
/// fold: 1 means folding constants, while 0 means not,
/// -1 means try to fold constants if without errors/warnings, otherwise not.
///
/// Deviations (D08.1 spec §4): `ret_type` is not optional (Go's nil check is the caller's
/// type), the error Go's `BuildCastFunction` logs is returned, extension functions
/// (`pkg/extension`) are not supported, and a family whose function classes are not ported
/// yet returns an error ([`crate::builtin_registry::function_class`]). Go keeps the
/// caller's `retType` as `RetType` and the signature's own `tp` apart when the signature's
/// type is `TypeUnspecified`; no v8.5.8 signature has such a type, and the node's one type
/// becomes the caller's here.
// go: newFunctionImpl L2 (see the doc comment)
pub fn new_function_impl(
    ctx: &dyn BuildContext,
    fold: i32,
    func_name: &str,
    ret_type: FieldType,
    check_or_init: Option<ScalarFunctionCallBack>,
    args: Vec<ExprRef>,
) -> Result<ExprRef, Error> {
    let ret = new_function_impl_inner(ctx, fold, func_name, ret_type, args)?;
    if let Some(check) = check_or_init
        && let Expr::ScalarFunction(sf) = &*ret
    {
        // Go passes the node itself, not a Clone: copy it field by field so that the
        // signature keeps its state (`ScalarFunction::clone` is Go's Clone, which drops e.g.
        // the `hasNullable` of builtinIntervalIntSig).
        return check(ScalarFunction {
            func_name: sf.func_name.clone(),
            function: sf.function.clone(),
            hashcode: sf.hashcode.clone(),
            canonicalhashcode: sf.canonicalhashcode.clone(),
        });
    }
    Ok(ret)
}

// glue: body of newFunctionImpl without its deferred checkOrInit
fn new_function_impl_inner(
    ctx: &dyn BuildContext,
    fold: i32,
    func_name: &str,
    mut ret_type: FieldType,
    args: Vec<ExprRef>,
) -> Result<ExprRef, Error> {
    let mut func_name = func_name;
    match func_name {
        ast_func::Cast => {
            return build_cast_function(ctx, Arc::clone(&args[0]), ret_type);
        }
        ast_func::GetVar => {
            // BuildGetVarFunction is D08.9.
            return Err(Error::plain(format!(
                "function {func_name} is not ported yet (D08.9)"
            )));
        }
        InternalFuncFromBinary => {
            return Ok(build_from_binary_function(
                ctx,
                Arc::clone(&args[0]),
                ret_type,
                false,
            ));
        }
        InternalFuncToBinary => return Ok(build_to_binary_function(ctx, Arc::clone(&args[0]))),
        ast_func::Sysdate if ctx.get_sysdate_is_now() => func_name = ast_func::Now,
        _ => {}
    }
    if lookup(func_name).is_none() {
        let db = ctx.get_eval_ctx().current_db();
        if db.is_empty() {
            return Err(ErrNoDB.clone());
        }
        return Err(ErrFunctionNotExists
            .gen_with_stack_by_args(vec!["FUNCTION".into(), format!("{db}.{func_name}").into()]));
    }
    let noop_funcs_mode = ctx.get_noop_funcs_mode();
    if noop_funcs_mode != OnInt && noopFuncs.contains(&func_name) {
        let err = ErrFunctionsNoopImpl.fast_gen_by_args(vec![func_name.to_string().into()]);
        if noop_funcs_mode == OffInt {
            return Err(err);
        }
        // NoopFuncsMode is Warn, append an error
        ctx.get_eval_ctx().append_warning(err);
    }
    let mut func_args = args;
    match func_name {
        ast_func::If | ast_func::Ifnull | ast_func::Nullif => {
            // Do nothing. Because it will call InferType4ControlFuncs.
        }
        ast_func::RowFunc => {
            // Do nothing. Because it shouldn't use ROW's args to infer null type.
        }
        _ => type_infer_for_null(&*ctx.get_eval_ctx(), &mut func_args),
    }

    let Some(fc) = function_class(func_name) else {
        return Err(Error::plain(format!(
            "the function class of {func_name} is not ported yet"
        )));
    };
    let mut f = fc.get_function(ctx, func_args)?;
    // §3.2: no v8.5.8 signature has a TypeUnspecified type, so the node's type and the
    // signature's are one (Lucky, review of #95).
    debug_assert_ne!(
        f.tp.get_type(),
        mysql::TypeUnspecified,
        "{func_name}: signature type TypeUnspecified (D08.1 spec §3.2)"
    );
    // Go's f.getRetTp(): a string type of flen >= 65536 becomes MEDIUMBLOB / LONGBLOB here.
    let builtin_ret_tp = f.get_ret_tp().clone();
    if builtin_ret_tp.get_type() != mysql::TypeUnspecified
        || ret_type.get_type() == mysql::TypeUnspecified
    {
        ret_type = builtin_ret_tp;
    }
    let sf = Expr::ScalarFunction(ScalarFunction::new(func_name, ret_type, f)).into_ref();
    if fold == 1 {
        return Ok(fold_constant(ctx, sf));
    } else if fold == -1 {
        // try to fold constants, and return the original function if errors/warnings occur
        let eval_ctx = ctx.get_eval_ctx();
        let before_warns = warning_count(&*eval_ctx);
        let new_sf = fold_constant(ctx, Arc::clone(&sf));
        let after_warns = warning_count(&*eval_ctx);
        if after_warns > before_warns {
            truncate_warnings(&*eval_ctx, before_warns);
            return Ok(sf);
        }
        return Ok(new_sf);
    }
    Ok(sf)
}

/// The check `NewFunction`, `NewFunctionBase` and `NewFunctionTryFold` run on the built
/// node. Go returns an error for a `grouping` function whose meta data is not initialized;
/// `BuiltinGroupingImplSig` is D08.9 and cannot be built yet, so no node reaches that check.
// go: defaultScalarFunctionCheck L2 (the grouping branch waits for D08.9)
pub fn default_scalar_function_check(function: ScalarFunction) -> Result<ExprRef, Error> {
    // todo: more scalar function init actions can be added here, or setting up with customized init callback.
    Ok(Expr::ScalarFunction(function).into_ref())
}

/// NewFunctionWithInit creates a new scalar function with callback init function.
// go: NewFunctionWithInit L1
pub fn new_function_with_init(
    ctx: &dyn BuildContext,
    func_name: &str,
    ret_type: FieldType,
    init: Option<ScalarFunctionCallBack>,
    args: Vec<ExprRef>,
) -> Result<ExprRef, Error> {
    new_function_impl(ctx, 1, func_name, ret_type, init, args)
}

/// NewFunction creates a new scalar function or constant via a constant folding.
// go: NewFunction L1
pub fn new_function(
    ctx: &dyn BuildContext,
    func_name: &str,
    ret_type: FieldType,
    args: Vec<ExprRef>,
) -> Result<ExprRef, Error> {
    new_function_impl(
        ctx,
        1,
        func_name,
        ret_type,
        Some(default_scalar_function_check),
        args,
    )
}

/// NewFunctionBase creates a new scalar function with no constant folding.
// go: NewFunctionBase L1
pub fn new_function_base(
    ctx: &dyn BuildContext,
    func_name: &str,
    ret_type: FieldType,
    args: Vec<ExprRef>,
) -> Result<ExprRef, Error> {
    new_function_impl(
        ctx,
        0,
        func_name,
        ret_type,
        Some(default_scalar_function_check),
        args,
    )
}

/// NewFunctionTryFold creates a new scalar function with trying constant folding.
// go: NewFunctionTryFold L1
pub fn new_function_try_fold(
    ctx: &dyn BuildContext,
    func_name: &str,
    ret_type: FieldType,
    args: Vec<ExprRef>,
) -> Result<ExprRef, Error> {
    new_function_impl(
        ctx,
        -1,
        func_name,
        ret_type,
        Some(default_scalar_function_check),
        args,
    )
}

/// NewFunctionInternal is similar to NewFunction, but do not return error, should only be
/// used internally. Go logs the error and returns the nil expression; this returns `None`.
// go: NewFunctionInternal L2 (None for Go's nil Expression; the error is logged)
pub fn new_function_internal(
    ctx: &dyn BuildContext,
    func_name: &str,
    ret_type: FieldType,
    args: Vec<ExprRef>,
) -> Option<ExprRef> {
    match new_function(ctx, func_name, ret_type, args) {
        Ok(e) => Some(e),
        Err(err) => {
            tidb_base::terror::log(Some(&err));
            None
        }
    }
}
