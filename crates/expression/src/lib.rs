//! Port of Go TiDB v8.5.8 `pkg/expression` (blocks D08.1, D08.2, D08.8, D08.12, D08.11).
//! N0 rank 8.
//!
//! So far: block D08.1 part 1 — the plan-side nodes `Column`, `CorrelatedColumn`,
//! `Constant` and `Schema`, their row evaluation, and the error and context helpers
//! (spec `specs/D08.1-expr`, design N4). `ScalarFunction` is a transitional shell until
//! part 2 (spec §3.3).

#![deny(unsafe_code)]

// `#[eval_fn]` refers to this crate as `::tidb_expression`, also from inside it.
#[allow(unused_extern_crates)]
extern crate self as tidb_expression;

pub mod collation;
pub mod column;
pub mod constant;
pub mod context;
pub mod errors;
pub mod eval;
pub mod expression;
pub mod scalar_function;
pub mod schema;
pub mod size;

pub use collation::*;
pub use column::*;
pub use constant::*;
pub use context::{BuildContext, EvalContext, ParamValues};
pub use errors::*;
pub use expression::*;
pub use scalar_function::{ScalarFunction, expressions_semantic_equal, re_hash_code, scalar_funcs_2_exprs};
pub use schema::*;
pub use tidb_expression_macros::eval_fn;
