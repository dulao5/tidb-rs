//! `pkg/store/copr`: the coprocessor client (block D07.2).
//!
//! Batch coprocessor and MPP (`batch_coprocessor.go`, `batch_request_sender.go`, `mpp*.go`)
//! are TiFlash only and belong to DEF.MPP; the coprocessor cache (`coprocessor_cache.go`)
//! is D07.4 (exception A3).

pub mod coprocessor;
pub mod key_ranges;
pub mod region_cache;
pub mod tikv;
