//! go-file: pkg/expression/builtin_vec.go
//! go-file: pkg/expression/builtin_vec_vec.go
//! go-file: pkg/expression/vs_helper.go
//! VECTOR functions (D08.1 part 3): `pkg/expression/builtin_vec.go`, the generated vector
//! versions in `builtin_vec_vec.go` and `vs_helper.go` (Go v8.5.8).
//!
//! VEC_DIMS, VEC_L2_NORM, VEC_FROM_TEXT and VEC_AS_TEXT are unary: the row and vector
//! versions do the same thing per row. The four distance functions take two vectors: the
//! row version returns NULL without evaluating `args[1]` when `args[0]` is NULL, the vector
//! version evaluates both columns and merges the NULLs, so their bodies are lazy (same as
//! LogicXor and the bit operators, D08.5 spec §3.2). A NaN distance is NULL in both.

#![allow(non_upper_case_globals)] // Go signature names (builtinXxxSig)

use tidb_base::Error;
use tidb_base::mysql;
use tidb_parser::model::CIStr;
use tidb_parser::types::{ETInt, ETReal, ETString, ETVectorFloat32};
use tidb_types::vector::{VectorFloat32, parse_vector_float32};

use crate::ast_funcs as ast;
use crate::builtin::{BaseFunctionClass, new_base_builtin_func_with_tp};
use crate::builtin_registry::FunctionClass;
use crate::column::Column;
use crate::context::{BuildContext, EvalContext};
use crate::eval::{Args, BuiltinFunc, SigDesc, SigView};
use crate::expression::{Expr, ExprRef};
use tidb_expression_macros::eval_fn;

// ---------------------------------------------------------------- function classes

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VecKind {
    Dims,
    L1Distance,
    L2Distance,
    NegativeInnerProduct,
    CosineDistance,
    L2Norm,
    FromText,
    AsText,
}

/// The function classes of the VECTOR functions (`vecDimsFunctionClass`,
/// `vecL1DistanceFunctionClass`, …): they differ only by the argument and result types and
/// the signature.
#[derive(Debug)]
pub struct VecFunctionClass {
    /// The embedded baseFunctionClass.
    pub base: BaseFunctionClass,
    kind: VecKind,
}

macro_rules! vec_class {
    ($name:ident, $fname:expr, $n:expr, $kind:expr) => {
        #[doc = concat!("The function class of `", stringify!($fname), "`.")]
        pub static $name: VecFunctionClass = VecFunctionClass {
            base: BaseFunctionClass {
                func_name: $fname,
                min_args: $n,
                max_args: $n,
            },
            kind: $kind,
        };
    };
}

vec_class!(VEC_DIMS_CLASS, ast::VecDims, 1, VecKind::Dims);
vec_class!(
    VEC_L1_DISTANCE_CLASS,
    ast::VecL1Distance,
    2,
    VecKind::L1Distance
);
vec_class!(
    VEC_L2_DISTANCE_CLASS,
    ast::VecL2Distance,
    2,
    VecKind::L2Distance
);
vec_class!(
    VEC_NEGATIVE_INNER_PRODUCT_CLASS,
    ast::VecNegativeInnerProduct,
    2,
    VecKind::NegativeInnerProduct
);
vec_class!(
    VEC_COSINE_DISTANCE_CLASS,
    ast::VecCosineDistance,
    2,
    VecKind::CosineDistance
);
vec_class!(VEC_L2_NORM_CLASS, ast::VecL2Norm, 1, VecKind::L2Norm);
vec_class!(VEC_FROM_TEXT_CLASS, ast::VecFromText, 1, VecKind::FromText);
vec_class!(VEC_AS_TEXT_CLASS, ast::VecAsText, 1, VecKind::AsText);

impl FunctionClass for VecFunctionClass {
    // go: vecDimsFunctionClass.getFunction L1
    // go: vecL1DistanceFunctionClass.getFunction L1
    // go: vecL2DistanceFunctionClass.getFunction L1
    // go: vecNegativeInnerProductFunctionClass.getFunction L1
    // go: vecCosineDistanceFunctionClass.getFunction L1
    // go: vecL2NormFunctionClass.getFunction L1
    // go: vecFromTextFunctionClass.getFunction L1
    // go: vecAsTextFunctionClass.getFunction L1
    fn get_function(
        &self,
        ctx: &dyn BuildContext,
        args: Vec<ExprRef>,
    ) -> Result<BuiltinFunc, Error> {
        self.base.verify_args(&args)?;
        let v = ETVectorFloat32;
        let (ret, arg_tps, sig): (_, &[_], &'static SigDesc) = match self.kind {
            VecKind::Dims => (ETInt, &[v], &builtinVecDimsSig),
            VecKind::L1Distance => (ETReal, &[v, v], &builtinVecL1DistanceSig),
            VecKind::L2Distance => (ETReal, &[v, v], &builtinVecL2DistanceSig),
            VecKind::NegativeInnerProduct => (ETReal, &[v, v], &builtinVecNegativeInnerProductSig),
            VecKind::CosineDistance => (ETReal, &[v, v], &builtinVecCosineDistanceSig),
            VecKind::L2Norm => (ETReal, &[v], &builtinVecL2NormSig),
            VecKind::FromText => (v, &[ETString], &builtinVecFromTextSig),
            VecKind::AsText => (ETString, &[v], &builtinVecAsTextSig),
        };
        let bf = new_base_builtin_func_with_tp(ctx, self.base.func_name, args, ret, arg_tps)?;
        Ok(bf.with_sig(sig))
    }
}

// ---------------------------------------------------------------- signatures

// go: builtinVecDimsSig.evalInt L1
// go: builtinVecDimsSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
// go: builtinVecDimsSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinVecDimsSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Int)]
pub fn vec_dims(
    _ctx: &dyn EvalContext,
    _f: &impl SigView,
    v: &VectorFloat32<'_>,
) -> Result<Option<i64>, Error> {
    Ok(Some(v.len() as i64))
}

/// The distance bodies: `args[1]` is not evaluated when `args[0]` is NULL (row version);
/// the vector driver evaluates both columns first (Go's `MergeNulls`). A distance error
/// (different dimensions) is returned; a NaN distance is NULL.
macro_rules! vec_distance {
    ($fn:ident, $method:ident) => {
        #[eval_fn(Real, lazy, args(VectorFloat32, VectorFloat32))]
        pub fn $fn(
            ctx: &dyn EvalContext,
            _f: &impl SigView,
            args: &mut impl Args,
        ) -> Result<Option<f64>, Error> {
            let Some(v1) = args
                .vector_float32(ctx, 0)?
                .map(|v| VectorFloat32::clone(&v))
            else {
                return Ok(None);
            };
            let Some(v2) = args.vector_float32(ctx, 1)? else {
                return Ok(None);
            };
            let d = v1.$method(&v2)?;
            if d.is_nan() {
                return Ok(None);
            }
            Ok(Some(d))
        }
    };
}

// go: builtinVecL1DistanceSig.evalReal L1
// go: builtinVecL1DistanceSig.vecEvalReal L2 (the vector driver of this lazy body over both evaluated columns)
// go: builtinVecL1DistanceSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinVecL1DistanceSig.vectorized L2 (SigDesc.vectorized)
vec_distance!(vec_l1_distance, l1_distance);
// go: builtinVecL2DistanceSig.evalReal L1
// go: builtinVecL2DistanceSig.vecEvalReal L2 (the vector driver of this lazy body over both evaluated columns)
// go: builtinVecL2DistanceSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinVecL2DistanceSig.vectorized L2 (SigDesc.vectorized)
vec_distance!(vec_l2_distance, l2_distance);
// go: builtinVecNegativeInnerProductSig.evalReal L1
// go: builtinVecNegativeInnerProductSig.vecEvalReal L2 (the vector driver of this lazy body over both evaluated columns)
// go: builtinVecNegativeInnerProductSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinVecNegativeInnerProductSig.vectorized L2 (SigDesc.vectorized)
vec_distance!(vec_negative_inner_product, negative_inner_product);
// go: builtinVecCosineDistanceSig.evalReal L1
// go: builtinVecCosineDistanceSig.vecEvalReal L2 (the vector driver of this lazy body over both evaluated columns)
// go: builtinVecCosineDistanceSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinVecCosineDistanceSig.vectorized L2 (SigDesc.vectorized)
vec_distance!(vec_cosine_distance, cosine_distance);

// go: builtinVecL2NormSig.evalReal L1
// go: builtinVecL2NormSig.vecEvalReal L2 (the vector driver of this body, M-EVAL)
// go: builtinVecL2NormSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinVecL2NormSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Real)]
pub fn vec_l2_norm(
    _ctx: &dyn EvalContext,
    _f: &impl SigView,
    v: &VectorFloat32<'_>,
) -> Result<Option<f64>, Error> {
    let d = v.l2_norm();
    if d.is_nan() {
        return Ok(None);
    }
    Ok(Some(d))
}

// go: builtinVecFromTextSig.evalVectorFloat32 L1
// go: builtinVecFromTextSig.vecEvalVectorFloat32 L2 (the vector driver of this body, M-EVAL)
// go: builtinVecFromTextSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinVecFromTextSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(VectorFloat32)]
pub fn vec_from_text(
    _ctx: &dyn EvalContext,
    f: &impl SigView,
    v: &[u8],
) -> Result<Option<VectorFloat32<'static>>, Error> {
    let vec = parse_vector_float32(v)?;
    vec.check_dims_fit_column(f.tp.get_flen())?;
    Ok(Some(vec))
}

// go: builtinVecAsTextSig.evalString L1
// go: builtinVecAsTextSig.vecEvalString L2 (the vector driver of this body, M-EVAL)
// go: builtinVecAsTextSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinVecAsTextSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(String)]
pub fn vec_as_text(
    _ctx: &dyn EvalContext,
    _f: &impl SigView,
    v: &VectorFloat32<'_>,
    out: &mut Vec<u8>,
) -> Result<Option<()>, Error> {
    out.extend_from_slice(v.string().as_bytes());
    Ok(Some(()))
}

macro_rules! vec_sig {
    ($name:ident, $pb:expr, $drivers:ident) => {
        #[doc = concat!("`", stringify!($name), "`.")]
        pub static $name: SigDesc = SigDesc {
            name: stringify!($name),
            pb_code: $pb,
            vectorized: true,
            drivers: $drivers,
            args_fallback: crate::eval::ArgsFallback::None,
        };
    };
}

// tipb.ScalarFuncSig codes (go-tipb expression.pb.go); Go comments the setPbCode of
// VecFromText out, so it has no code.
vec_sig!(builtinVecDimsSig, 5112, VEC_DIMS);
vec_sig!(builtinVecL1DistanceSig, 5113, VEC_L1_DISTANCE);
vec_sig!(builtinVecL2DistanceSig, 5114, VEC_L2_DISTANCE);
vec_sig!(
    builtinVecNegativeInnerProductSig,
    5115,
    VEC_NEGATIVE_INNER_PRODUCT
);
vec_sig!(builtinVecCosineDistanceSig, 5116, VEC_COSINE_DISTANCE);
vec_sig!(builtinVecL2NormSig, 5117, VEC_L2_NORM);
vec_sig!(builtinVecFromTextSig, 0, VEC_FROM_TEXT);
vec_sig!(builtinVecAsTextSig, 5110, VEC_AS_TEXT);

// ---------------------------------------------------------------- vs_helper.go

/// VectorHelper is a helper struct for vector indexes.
#[derive(Debug)]
pub struct VectorHelper {
    /// DistanceFnName.
    pub distance_fn_name: CIStr,
    /// FnPbCode.
    pub fn_pb_code: i32,
    /// Vec.
    pub vec: VectorFloat32<'static>,
    /// Column.
    pub column: Column,
}

/// The lower-case names of the distance functions a vector index can serve
/// (`vsDistanceFnNamesLower`).
const VS_DISTANCE_FN_NAMES_LOWER: [&str; 4] = [
    ast::VecL1Distance,
    ast::VecL2Distance,
    ast::VecCosineDistance,
    ast::VecNegativeInnerProduct,
];

/// ExtractVectorHelper extracts a VectorSearchExpr from an expression.
/// NOTE: not all VectorSearch functions are supported by the index. The caller
/// needs to check the distance function name.
// The intest assertion on the constant's kind is compiled out of production Go builds (also here).
// go: ExtractVectorHelper L1
pub fn extract_vector_helper(expr: &ExprRef) -> Option<VectorHelper> {
    let Expr::ScalarFunction(x) = &**expr else {
        return None;
    };
    if !VS_DISTANCE_FN_NAMES_LOWER.contains(&x.func_name.l.as_str()) {
        return None;
    }
    // One arg must be a vector column ref, and one arg must be a vector constant.
    let mut vector_constant = None;
    let mut vector_column = None;
    let mut n_vector_columns = 0;
    let mut n_vector_constants = 0;
    for arg in x.get_args() {
        match &**arg {
            Expr::Column(v) => {
                if v.ret_type.get_type() != mysql::TypeTiDBVectorFloat32 {
                    return None;
                }
                vector_column = Some(v);
                n_vector_columns += 1;
            }
            Expr::Constant(v) => {
                if v.ret_type.get_type() != mysql::TypeTiDBVectorFloat32 {
                    return None;
                }
                vector_constant = Some(v);
                n_vector_constants += 1;
            }
            _ => {}
        }
    }
    if n_vector_columns != 1 || n_vector_constants != 1 {
        return None;
    }
    let (vector_constant, vector_column) = (vector_constant?, vector_column?);
    Some(VectorHelper {
        distance_fn_name: x.func_name.clone(),
        fn_pb_code: x.function.pb_code(),
        vec: VectorFloat32::clone(&vector_constant.value.get_vector_float32()),
        column: vector_column.clone(),
    })
}
