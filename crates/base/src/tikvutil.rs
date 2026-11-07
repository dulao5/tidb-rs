//! go-file: pkg/util/tikvutil/tikvutil.go
//!
//! `pkg/util/tikvutil` (block D00.2): one variable, read by `config.GetTiKVConfig`.

// Package-level vars keep their Go names (mapping rule M-GONAME).
#![allow(non_upper_case_globals)]

use std::sync::atomic::AtomicI32;

/// CommitterConcurrency is the number of goroutines used to commit a transaction (Go:
/// `atomic.NewInt32(128)`).
pub static CommitterConcurrency: AtomicI32 = AtomicI32::new(128);
