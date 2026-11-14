//! Port of Go TiDB v8.5.8 `pkg/expression` push-down (block D08.3). N0 rank 12: the
//! push-down tables name the functions of every family by signature, so they sit above the
//! function families instead of inside the expression framework (N0 §3).
//!
//! Part 1 (spec `specs/D08.3-pushdown`): push-down eligibility (`infer_pushdown.go`) and the
//! conversion of expressions to tipb (`expr_to_pb.go`). Part 2 is the reverse direction
//! (`distsql_builtin.go`, `PBToExpr`).

#![deny(unsafe_code)]

pub mod expr_to_pb;
pub mod infer_pushdown;
pub mod tipb_sig;

pub use expr_to_pb::*;
pub use infer_pushdown::*;
