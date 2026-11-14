//! Stand-in for `pkg/ddl/placement` (tracker block X: placement rules are written by the
//! DDL owner and PD). The InfoSchema keeps a bundle per physical table that has a placement
//! policy; on the read path it is read only by `distsql.VerifyTxnScope` with a local
//! transaction scope, which tidb-rs does not support (no local TSO). The bundle content
//! (`NewBundleFromOptions`) is X, so a bundle here records only which IDs it covers and the
//! policy it came from.

use std::sync::Arc;

use tidb_base::Error;
use tidb_model::{PartitionDefinition, PolicyInfo, PolicyRefInfo, TableInfo};

/// PolicyGetter is the interface to get the policy.
// glue: stand-in: X, placement.PolicyGetter
pub trait PolicyGetter {
    // glue: stand-in: X, interface method PolicyGetter.GetPolicy
    fn get_policy(&self, policy_id: i64) -> Result<Arc<PolicyInfo>, Error>;
}

/// Bundle is a group of all rules and configurations. It is used to support rule cache.
// glue: stand-in: X, placement.Bundle (the rules are not built)
#[derive(Clone, Debug, PartialEq)]
pub struct Bundle {
    /// The policy the bundle was built from.
    pub policy_id: i64,
    /// The physical IDs the bundle's rules cover (`Bundle.Reset(index, ids)`).
    pub ids: Vec<i64>,
}

/// newBundleFromPolicy: Go reads the policy (its error is returned) and builds the rules
/// from its settings (X).
// glue: stub, X (placement.newBundleFromPolicy; the rules are not built)
fn new_bundle_from_policy(
    getter: &dyn PolicyGetter,
    r: Option<&PolicyRefInfo>,
) -> Result<Option<Bundle>, Error> {
    if let Some(r) = r {
        let policy = getter.get_policy(r.id)?;
        return Ok(Some(Bundle {
            policy_id: policy.id,
            ids: Vec::new(),
        }));
    }
    Ok(None)
}

/// NewTableBundle creates a bundle for table key range.
// glue: stub, X (placement.NewTableBundle; the IDs as Go's, no rules)
pub fn new_table_bundle(
    getter: &dyn PolicyGetter,
    tb_info: &TableInfo,
) -> Result<Option<Bundle>, Error> {
    let Some(mut bundle) = new_bundle_from_policy(getter, tb_info.placement_policy_ref.as_deref())?
    else {
        return Ok(None);
    };
    let mut ids = vec![tb_info.id];
    // build the default partition rules in the table-level bundle.
    if let Some(p) = &tb_info.partition {
        for p_def in p.definitions.iter().flatten() {
            ids.push(p_def.id);
        }
    }
    bundle.ids = ids;
    Ok(Some(bundle))
}

/// NewPartitionBundle creates a bundle for partition key range.
// glue: stub, X (placement.NewPartitionBundle; the IDs as Go's, no rules)
pub fn new_partition_bundle(
    getter: &dyn PolicyGetter,
    def: &PartitionDefinition,
) -> Result<Option<Bundle>, Error> {
    let mut bundle = new_bundle_from_policy(getter, def.placement_policy_ref.as_deref())?;
    if let Some(b) = &mut bundle {
        b.ids = vec![def.id];
    }
    Ok(bundle)
}
