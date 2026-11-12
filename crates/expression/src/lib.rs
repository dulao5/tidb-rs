//! Port of Go TiDB v8.5.8 `pkg/expression` (blocks D08.1, D08.2, D08.8, D08.12, D08.11).
//! N0 rank 8.
//!
//! So far: block D08.1 parts 1 and 2a — the nodes `Column`, `CorrelatedColumn`,
//! `Constant`, `ScalarFunction` and `Schema`, the evaluation framework (spec
//! `specs/D08.1-expr`, design N4) — and block D08.2 part 1: the `funcs` registry, building a
//! signature's base (type, collation, NOT NULL flag) and the collation derivation of
//! functions (spec `specs/D08.2-funcs`) — and block D08.8 (CAST) parts 1, 2a and 2b-1: CAST to
//! INT/REAL/DECIMAL/STRING, the internal to_binary/from_binary functions and constant folding
//! (spec `specs/D08.8-cast`).

#![deny(unsafe_code)]

// `#[eval_fn]` refers to this crate as `::tidb_expression`, also from inside it.
#[allow(unused_extern_crates)]
extern crate self as tidb_expression;

/// The function-name constants of `pkg/parser/ast/functions.go`, generated into tidb-parser
/// (the parser uses them too); re-exported here so callers keep `ast_funcs`.
pub use tidb_parser::ast::funcs as ast_funcs;
pub mod builtin;
pub mod builtin_arithmetic;
pub mod builtin_cast;
pub mod builtin_compare;
pub mod builtin_convert_charset;
pub mod builtin_math;
pub mod builtin_registry;
mod builtin_registry_gen;
pub mod chunk_executor;
pub mod collation;
pub mod column;
pub mod constant;
pub mod constant_fold;
pub mod context;
pub mod errors;
pub mod eval;
pub mod evaluator;
pub mod expression;
pub mod function_traits;
pub mod scalar_function;
pub mod schema;
pub mod size;
pub mod util;

pub use collation::*;
pub use column::*;
pub use constant::*;
pub use context::{BuildContext, EvalContext, ParamValues};
pub use errors::*;
pub use expression::*;
pub use scalar_function::{
    ScalarFunction, expressions_semantic_equal, re_hash_code, scalar_funcs_2_exprs,
};
pub use schema::*;
pub use tidb_expression_macros::eval_fn;
