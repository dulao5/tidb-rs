//! Port of Go TiDB v8.5.8 `pkg/util/execdetails` and `pkg/util/tracing` (block D14.0):
//! execution details and runtime statistics, and the optimizer trace structures.
//!
//! Also the client-go `util` detail types ([`clientgo`]) and the t-digest ([`tdigest`])
//! that execdetails embeds (dependency ports).

// Go identifiers for constants (mapping rule M-GONAME).
#![allow(non_upper_case_globals)]

pub mod clientgo;
pub mod execdetails;
pub mod tdigest;
pub mod tracing;
