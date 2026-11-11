//! The ranger types (Go `pkg/util/ranger` `types.go`, the full ranges of `points.go`, and
//! `pkg/util/ranger/context`). Block D00.1 (N0 review: `ranger.Range` is the type that the
//! distsql layer and the planner share; range building itself is D10.4).

pub mod context;
pub mod types;

pub use self::types::*;
