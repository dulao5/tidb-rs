//! Port of Go TiDB v8.5.8 `pkg/expression` push-down (block D08.3). N0 rank 12: the
//! push-down tables name the functions of every family by signature, so they sit above the
//! function families instead of inside the expression framework (N0 §3).
//!
//! Spec `specs/D08.3-pushdown`: push-down eligibility (`infer_pushdown.go`), the conversion
//! of expressions to tipb (`expr_to_pb.go`, part 1) and the reverse direction
//! (`distsql_builtin.go`, `PBToExpr`, part 2).

#![deny(unsafe_code)]

pub mod distsql_builtin;
pub mod expr_to_pb;
pub mod infer_pushdown;
pub mod tipb_sig;

pub use distsql_builtin::*;
pub use expr_to_pb::*;
pub use infer_pushdown::*;
