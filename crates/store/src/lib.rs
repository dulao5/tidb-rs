//! Port of Go TiDB v8.5.8 `pkg/store` (blocks D07.1–D07.4, N0 rank 11).
//!
//! So far: `pkg/store/driver`, the store driver's point reads (block D07.1 part 1: snapshot
//! `Get` / `BatchGet` / `SetOption`, the error conversion, the backoffer; VS1-18), and
//! `pkg/store/copr`, the coprocessor client (block D07.2; spec `specs/D07.2-copr/`), with the
//! util file `pkg/util/paging` it needs. The TiKV client is the client-rust fork (ADR 0008),
//! reached through the [`driver::tikv`] and [`copr::tikv`] boundaries.

pub mod copr;
pub mod driver;
pub mod util;
