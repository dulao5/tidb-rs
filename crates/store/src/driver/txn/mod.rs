//! `pkg/store/driver/txn`: the snapshot, the buffer batch getter and the key errors (block
//! D07.1 part 1, VS1-18). The transaction (`txn_driver.go`), the scanner and the union
//! store are D07.1 part 2.

pub mod batch_getter;
pub mod error;
pub mod snapshot;
