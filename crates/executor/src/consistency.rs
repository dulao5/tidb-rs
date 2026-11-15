//! go-file: pkg/util/logutil/consistency/reporter.go
//!
//! Go package `pkg/util/logutil/consistency` (block D00.3): the error and log of a data
//! inconsistency found while reading. So far the report of the point get (VS1-11): an
//! index entry whose row is missing.
//!
//! The executor is the only user so far, so the module lives here; it moves to the util
//! crate with the rest of D00.3.
//!
//! Go dumps the MVCC history of the missing row and of the index entry into the log when the
//! storage is a `helper.Storage` (the TiKV store with the MVCC debug API). The tidb-rs store
//! has no such API, so the dump is never taken (Go's `!ok` branch).

#![allow(non_upper_case_globals)]

use std::sync::LazyLock;

use tidb_base::Error;
use tidb_base::dbterror::ClassExecutor;
use tidb_base::errno;
use tidb_base::errors::RedactLogEnable;
use tidb_base::redact;
use tidb_kv::key::Handle;
use tidb_model::{IndexInfo, TableInfo};

/// ErrLookupInconsistent is the error for data inconsistency during index lookup.
pub static ErrLookupInconsistent: LazyLock<Error> =
    LazyLock::new(|| ClassExecutor.new_std(errno::ErrDataInconsistentMismatchCount));

/// Reporter is a helper to generate report.
///
/// Go's `HandleEncode` / `IndexEncode` / `Storage` serve the MVCC dump only (see the module
/// doc) and are not kept.
pub struct Reporter<'a> {
    pub tbl: &'a TableInfo,
    pub idx: &'a IndexInfo,
    pub enable_redact_log: String,
}

/// `fmt.Sprint` of a `[]kv.Handle`: `[h1 h2]`.
// glue: fmt.Sprint of a handle slice
fn sprint_handles(hs: &[Handle]) -> String {
    let parts: Vec<String> = hs.iter().map(|h| h.to_string()).collect();
    format!("[{}]", parts.join(" "))
}

impl Reporter<'_> {
    /// ReportLookupInconsistent reports inconsistent when index rows is more than record rows.
    ///
    /// `miss_row_idx` (Go's `[]RecordData`) only feeds the index MVCC dump, which is not
    /// taken (see the module doc).
    // go: Reporter.ReportLookupInconsistent L2 (no MVCC dump: the store is not a helper.Storage)
    pub fn report_lookup_inconsistent(
        &self,
        idx_cnt: i64,
        tbl_cnt: i64,
        miss_hd: &[Handle],
        full_hd: &[Handle],
        miss_row_idx: usize,
    ) -> Error {
        let rmode = self.enable_redact_log.as_str();

        const MAX_FULL_HANDLE_CNT: usize = 50;
        let display_full_hd_cnt = full_hd.len().min(MAX_FULL_HANDLE_CNT);
        // if rmode != errors.RedactLogEnable: r.Storage.(helper.Storage) is never ok here.
        let _ = (rmode != RedactLogEnable, miss_row_idx);
        log::error!(
            "indexLookup found data inconsistency, table_name: {}, index_name: {}, index_cnt: {}, table_cnt: {}, missing_handles: {}, total_handles: {}",
            self.tbl.name.o,
            self.idx.name.o,
            idx_cnt,
            tbl_cnt,
            redact::string(rmode, &sprint_handles(miss_hd)),
            redact::string(rmode, &sprint_handles(&full_hd[..display_full_hd_cnt])),
        );
        ErrLookupInconsistent.gen_with_stack_by_args(tidb_base::go_args![
            self.tbl.name.o.clone(),
            self.idx.name.o.clone(),
            idx_cnt,
            tbl_cnt
        ])
    }
}
