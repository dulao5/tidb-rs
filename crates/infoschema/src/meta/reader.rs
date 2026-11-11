//! go-file: pkg/meta/reader.go
//!
//! `meta.Reader`: the read-only view of the meta data at a snapshot. Go's `Reader` is an
//! interface implemented by `*Mutator`; tidb-rs has only the read side, so [`new_reader`]
//! returns the [`Mutator`] itself (its methods are the interface's).

use std::sync::Arc;

use tidb_kv::kv::Snapshot;
use tidb_kv::option::InternalTxnMeta;
use tidb_kv::option::RequestSourceInternal;
use tidb_kv::option::RequestSourceType;

use super::Mutator;
use super::mMetaPrefix;
use crate::structure::new_structure;

/// NewReader creates a meta Reader in snapshot.
// go: NewReader L1
pub fn new_reader(snapshot: Arc<dyn Snapshot>) -> Mutator {
    snapshot.set_option(RequestSourceInternal, Box::new(true));
    snapshot.set_option(RequestSourceType, Box::new(InternalTxnMeta.to_string()));
    let t = new_structure(snapshot, mMetaPrefix);
    Mutator {
        txn: t,
        start_ts: 0,
        job_list_key: Vec::new(),
    }
}
