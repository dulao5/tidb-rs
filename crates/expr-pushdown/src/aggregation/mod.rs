//! The functions of Go TiDB v8.5.8 `pkg/expression/aggregation` (block D08.11) that convert
//! descriptors to and from tipb and decide push-down: they call D08.3 (the expression
//! conversion, `IsPushDownEnabled`), so N0 rule 6 places them here as free functions, one
//! layer above the descriptors in tidb-expression (D08.11 spec §4.8). The tracker keeps them
//! in block D08.11 (decision point 3).
//!
//! So far part B: `agg_to_pb.go` and the push-down checks of `aggregation.go`.

pub mod agg_to_pb;
// The Go package `aggregation` has a file `aggregation.go` (file-for-file, constitution §2).
#[allow(clippy::module_inception)]
pub mod aggregation;

pub use agg_to_pb::*;
pub use aggregation::*;
