//! go-file: pkg/meta/model/reorg.go
//!
//! The reorg meta getters the read path keeps (block D06.1 part 2): `ADMIN SHOW DDL JOBS`
//! shows the concurrency, batch size and write speed of a running reorg.
//!
//! Mapping: `Concurrency`, `BatchSize` and `MaxWriteSpeed` are `go.uber.org/atomic.Int64`
//! in Go (`ADMIN ALTER DDL JOBS` changes them while the DDL worker runs); tidb-rs only reads
//! the job meta it decoded, so they are plain `i64` (spec D06.1 3.4; their JSON form, which
//! goes through the atomic's `MarshalJSON` / `UnmarshalJSON`, is Go's).

#![allow(non_upper_case_globals)]

use crate::{DDLReorgMeta, ReorgType, ReorgTypeIngest, ReorgTypeTxn, ReorgTypeTxnMerge};

impl DDLReorgMeta {
    /// GetConcurrencyOrDefault gets the concurrency from DDLReorgMeta,
    /// pass the default value in case of the reorg meta coming from old cluster and Concurrency is 0.
    // go: DDLReorgMeta.GetConcurrencyOrDefault L1 [M-INT]
    pub fn get_concurrency_or_default(&self, default_val: isize) -> isize {
        let concurrency = self.concurrency;
        if concurrency == 0 {
            return default_val;
        }
        concurrency as isize
    }

    /// GetBatchSizeOrDefault gets the batch size from DDLReorgMeta.
    // go: DDLReorgMeta.GetBatchSizeOrDefault L1 [M-INT]
    pub fn get_batch_size_or_default(&self, default_val: isize) -> isize {
        let batch_size = self.batch_size;
        if batch_size == 0 {
            return default_val;
        }
        batch_size as isize
    }

    /// GetMaxWriteSpeedOrDefault gets the max write speed from DDLReorgMeta.
    /// 0 means no limit.
    // go: DDLReorgMeta.GetMaxWriteSpeedOrDefault L1 [M-INT]
    pub fn get_max_write_speed_or_default(&self) -> isize {
        self.max_write_speed as isize
    }
}

impl ReorgType {
    /// String implements fmt.Stringer interface.
    // go: ReorgType.String L1 [M-NAME]
    pub fn to_string(self) -> &'static str {
        match self {
            ReorgTypeTxn => "txn",
            ReorgTypeIngest => "ingest",
            ReorgTypeTxnMerge => "txn-merge",
            _ => "",
        }
    }
}
