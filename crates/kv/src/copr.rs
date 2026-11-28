//! Coprocessor-shared KV types sunk from `tidb-store::copr` to `tidb-kv` (N0 §4 rule 1,
//! D07.3 spec §2.2 & §4.3):
//! - [`Variables`] (`tikv.Variables`) and [`RateLimit`] (`util.RateLimit`), held by
//!   `DistSQLContext` (rank 7) and `kv::Request` (rank 5);
//! - [`CopRuntimeStats`], returned through the default trait methods on
//!   [`crate::kv::ResultSubset`] and [`crate::kv::Response`];
//! - [`StoreLabel`] (`metapb.StoreLabel` value carrier for `Request.match_store_labels`);
//! - [`ResourceGroupTagBuilder`] (`pkg/kv/kv.go:836-900`, D07.0 T8 / #416).

use std::sync::Arc;
use std::sync::atomic::AtomicU32;

use tidb_runtime::KillCancel;

/// `tikv.Variables`: the per-session values the client reads.
#[derive(Clone, Debug)]
pub struct Variables {
    /// `BackoffWeight`: multiplies every backoff budget (`tidb_backoff_weight`, default 2).
    pub backoff_weight: i64,
    /// `Killed`: non-zero once the statement is killed.
    pub killed: Option<Arc<AtomicU32>>,
    /// Session-level wakeup handle paired with `killed`.
    pub kill_cancel: Option<KillCancel>,
}

impl Default for Variables {
    // go-l3: ADR-0008 client-go kv NewVariables
    fn default() -> Self {
        Variables {
            backoff_weight: 2,
            killed: None,
            kill_cancel: None,
        }
    }
}

/// client-go `util.RateLimit`: a token bucket of fixed capacity.
pub struct RateLimit {
    sem: tokio::sync::Semaphore,
    capacity: usize,
}

impl RateLimit {
    // go-l3: ADR-0008 client-go util NewRateLimit
    pub fn new(n: usize) -> RateLimit {
        if n == 0 {
            panic!("invalid rate limit capacity");
        }
        RateLimit {
            sem: tokio::sync::Semaphore::new(n),
            capacity: n,
        }
    }

    /// `GetToken(done)`: waits for a token; `true` (exit) when `done` fires first.
    // go-l3: ADR-0008 client-go util RateLimit.GetToken
    pub async fn get_token(&self, done: &mut tokio::sync::watch::Receiver<bool>) -> bool {
        if *done.borrow() {
            return true;
        }
        tokio::select! {
            permit = self.sem.acquire() => {
                permit.expect("rate limit semaphore closed").forget();
                false
            }
            _ = done.wait_for(|d| *d) => true,
        }
    }

    // go-l3: ADR-0008 client-go util RateLimit.PutToken
    pub fn put_token(&self) {
        self.sem.add_permits(1);
    }

    // go-l3: ADR-0008 client-go util RateLimit.GetCapacity
    pub fn get_capacity(&self) -> usize {
        self.capacity
    }
}

/// CopRuntimeStats contains execution detail information.
///
/// `ReqStats` (client-go `RegionRequestRuntimeStats`) has no client-rust counterpart and is
/// never set (D07.2 spec §6).
#[derive(Clone, Debug, Default)]
pub struct CopRuntimeStats {
    pub cop_exec_details: tidb_execdetails::execdetails::CopExecDetails,
    pub copr_cache_hit: bool,
}

/// StoreLabel mirrors kvproto `metapb.StoreLabel` without pulling `tikv-client` into
/// `tidb-kv` (N0 rank 5).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StoreLabel {
    pub key: String,
    pub value: String,
}

/// ResourceGroupTagBuilder builds a `tipb.ResourceGroupTag` for KV / coprocessor requests
/// (Go `pkg/kv/kv.go:836-900`, D07.0 T8).
///
/// `sql_digest` and `plan_digest` remain `None` until `parser.Digest` (D02.2) is ported
/// (tracking issue `#416`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResourceGroupTagBuilder {
    sql_digest: Option<Vec<u8>>,
    plan_digest: Option<Vec<u8>>,
}

/// NewResourceGroupTagBuilder creates a new ResourceGroupTagBuilder.
// go: NewResourceGroupTagBuilder L1
pub fn new_resource_group_tag_builder() -> ResourceGroupTagBuilder {
    ResourceGroupTagBuilder::default()
}

impl ResourceGroupTagBuilder {
    /// SetSQLDigest sets the sql digest for the request.
    // go: ResourceGroupTagBuilder.SetSQLDigest L2 (raw digest bytes until D02.2 parser.Digest)
    pub fn set_sql_digest(&mut self, digest: Option<Vec<u8>>) -> &mut Self {
        self.sql_digest = digest;
        self
    }

    /// SetPlanDigest sets the plan digest for the request.
    // go: ResourceGroupTagBuilder.SetPlanDigest L2 (raw digest bytes until D02.2 parser.Digest)
    pub fn set_plan_digest(&mut self, digest: Option<Vec<u8>>) -> &mut Self {
        self.plan_digest = digest;
        self
    }

    /// Configured SQL digest bytes, if any.
    // glue: field accessor
    pub fn sql_digest(&self) -> Option<&[u8]> {
        self.sql_digest.as_deref()
    }

    /// Configured plan digest bytes, if any.
    // glue: field accessor
    pub fn plan_digest(&self) -> Option<&[u8]> {
        self.plan_digest.as_deref()
    }

    /// Encode `tipb::ResourceGroupTag` using key-derived `(table_id, label)` supplied by the
    /// caller (`tidb-tablecodec` sits at rank 6 > `tidb-kv` rank 5, so callers above rank 6
    /// decode the table ID and key kind and pass them in, avoiding Go's package-global
    /// `DecodeTableIDFunc` mutable callback).
    ///
    /// Matches Go gogoproto wire encoding: `table_id` has `(gogoproto.nullable) = false`, so
    /// field 4 is always written (even when 0); `label` is nullable and set only when `key` is
    /// non-empty.
    // go: ResourceGroupTagBuilder.EncodeTagWithKey L2 (table_id and label passed by caller instead of global DecodeTableIDFunc)
    pub fn encode_tag_with_key_parts(
        &self,
        key: &[u8],
        table_id: i64,
        label: tipb::ResourceGroupTagLabel,
    ) -> Vec<u8> {
        let mut tag = tipb::ResourceGroupTag::default();
        if let Some(d) = &self.sql_digest
            && !d.is_empty()
        {
            tag.set_sql_digest(d.clone());
        }
        if let Some(d) = &self.plan_digest
            && !d.is_empty()
        {
            tag.set_plan_digest(d.clone());
        }
        if !key.is_empty() {
            tag.set_table_id(table_id);
            tag.set_label(label);
        } else {
            // Go gogoproto `optional int64 table_id = 4 [(gogoproto.nullable) = false]`
            // always serializes field 4 even when 0.
            tag.set_table_id(0);
        }
        protobuf::Message::write_to_bytes(&tag).unwrap_or_default()
    }
}
