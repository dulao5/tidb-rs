//! go-file: pkg/sessiontxn/internal/txn.go
//!
//! `SetTxnAssertionLevel` is the write path (assertions are checked on mutations).

use std::sync::Arc;

use tidb_base::Error;
use tidb_kv::kv::Snapshot;
use tidb_kv::option::{
    ExplicitRequestSourceType, LoadBasedReplicaReadThreshold, RequestSourceInternal,
    RequestSourceType, SnapInterceptor,
};
use tidb_kv::version::Version;

use crate::sctx::SessionCtx;

/// CommitBeforeEnterNewTxn is called before entering a new transaction. It checks whether
/// the old txn is valid in which case we should commit it first.
// go: CommitBeforeEnterNewTxn L1
pub async fn commit_before_enter_new_txn(sctx: &mut dyn SessionCtx) -> Result<(), Error> {
    let txn = sctx.txn();
    if txn.valid() {
        let txn_start_ts = txn.start_ts();
        let txn_scope = sctx.get_session_vars().state.txn_ctx.txn_scope.clone();
        sctx.commit_txn().await?;
        log::info!(
            "Try to create a new txn inside a transaction auto commit, schemaVersion: {}, txnStartTS: {txn_start_ts}, txnScope: {txn_scope}",
            sctx.get_domain_info_schema().base().schema_meta_version()
        );
    }
    Ok(())
}

/// GetSnapshotWithTS returns a snapshot with ts. The interceptor is the session's
/// temporary-table interceptor (temporary tables are D06.4 part 2: `None` until then).
// go: GetSnapshotWithTS L1
pub fn get_snapshot_with_ts(
    s: &dyn SessionCtx,
    ts: u64,
    interceptor: Option<tidb_kv::kv::OptionValue>,
) -> Arc<dyn Snapshot> {
    let snap = s.get_store().get_snapshot(Version { ver: ts });
    if let Some(interceptor) = interceptor {
        snap.set_option(SnapInterceptor, interceptor);
    }
    let vars = s.get_session_vars();
    if vars.in_restricted_sql {
        snap.set_option(RequestSourceInternal, Box::new(true));
    }
    let tp = vars.request_source_type.clone();
    if !tp.is_empty() {
        snap.set_option(RequestSourceType, Box::new(tp));
    }
    let tp = vars.explicit_request_source_type.clone();
    if !tp.is_empty() {
        snap.set_option(ExplicitRequestSourceType, Box::new(tp));
    }
    if vars.load_based_replica_read_threshold > 0 {
        snap.set_option(
            LoadBasedReplicaReadThreshold,
            Box::new(vars.load_based_replica_read_threshold),
        );
    }
    snap
}
