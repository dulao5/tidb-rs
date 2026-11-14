//! Port of Go TiDB v8.5.8 `pkg/store` (blocks D07.1–D07.4, N0 rank 11).
//!
//! So far: `pkg/store/copr`, the coprocessor client (block D07.2, part 1: key ranges, task
//! building, paging; spec `specs/D07.2-copr/`), and the util file `pkg/util/paging` it
//! needs. The TiKV client is the client-rust fork (ADR 0008), reached through the
//! [`copr::tikv`] boundary.

pub mod copr;
pub mod util;
