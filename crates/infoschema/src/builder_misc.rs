//! go-file: pkg/infoschema/builder_misc.go
//!
//! Placement policies, resource groups and temporary tables of the builder.

use std::collections::HashSet;
use std::sync::Arc;

use tidb_base::Error;
use tidb_base::gofmt::Arg;
use tidb_model::{PolicyInfo, ResourceGroupInfo, SchemaDiff};

use crate::builder::Builder;
use crate::error::{ErrPlacementPolicyNotExists, ErrResourceGroupNotExists};
use crate::meta::Mutator;

// go: applyCreatePolicy L1
pub(crate) async fn apply_create_policy(
    b: &mut Builder,
    m: &Mutator,
    diff: &SchemaDiff,
) -> Result<(), Error> {
    let (po, err) = m.get_policy(diff.schema_id).await;
    err?;
    let Some(po) = po else {
        return Err(ErrPlacementPolicyNotExists
            .gen_with_stack_by_args(vec![Arg::from(format!("(Policy ID {})", diff.schema_id))]));
    };

    if b.is.misc.policy_by_id(po.id).is_some() {
        // if old policy with the same id exists, it means replace,
        // so the tables referring this policy's bundle should be updated
        b.bundle.mark_bundles_refer_policy_should_update(po.id);
    }

    Arc::make_mut(&mut b.is.misc).set_policy(Arc::new(po));
    Ok(())
}

// go: applyAlterPolicy L1
pub(crate) async fn apply_alter_policy(
    b: &mut Builder,
    m: &Mutator,
    diff: &SchemaDiff,
) -> Result<Vec<i64>, Error> {
    let (po, err) = m.get_policy(diff.schema_id).await;
    err?;

    let Some(po) = po else {
        return Err(ErrPlacementPolicyNotExists
            .gen_with_stack_by_args(vec![Arg::from(format!("(Policy ID {})", diff.schema_id))]));
    };

    let id = po.id;
    Arc::make_mut(&mut b.is.misc).set_policy(Arc::new(po));
    b.bundle.mark_bundles_refer_policy_should_update(id);
    // TODO: return the policy related table ids
    Ok(Vec::new())
}

// go: applyDropPolicy L1
pub(crate) fn apply_drop_policy(b: &mut Builder, policy_id: i64) -> Vec<i64> {
    let Some(po) = b.is.misc.policy_by_id(policy_id) else {
        return Vec::new();
    };
    Arc::make_mut(&mut b.is.misc).delete_policy(&po.name.l);
    // TODO: return the policy related table ids
    Vec::new()
}

// go: applyCreateOrAlterResourceGroup L1
pub(crate) async fn apply_create_or_alter_resource_group(
    b: &mut Builder,
    m: &Mutator,
    diff: &SchemaDiff,
) -> Result<(), Error> {
    let (group, err) = m.get_resource_group(diff.schema_id).await;
    err?;
    let Some(group) = group else {
        return Err(ErrResourceGroupNotExists
            .gen_with_stack_by_args(vec![Arg::from(format!("(Group ID {})", diff.schema_id))]));
    };
    // TODO: need mark updated?
    Arc::make_mut(&mut b.is.misc).set_resource_group(Arc::new(group));
    Ok(())
}

// go: applyDropResourceGroup L1
pub(crate) fn apply_drop_resource_group(
    b: &mut Builder,
    _m: &Mutator,
    diff: &SchemaDiff,
) -> Vec<i64> {
    let Some(group) = b.is.misc.resource_group_by_id(diff.schema_id) else {
        return Vec::new();
    };
    Arc::make_mut(&mut b.is.misc).delete_resource_group(&group.name.l);
    // TODO: return the related information.
    Vec::new()
}

impl Builder {
    // go: Builder.addTemporaryTable L1
    pub(crate) fn add_temporary_table(&mut self, tbl_id: i64) {
        Arc::make_mut(&mut self.is.misc)
            .temporary_table_ids
            .get_or_insert_with(HashSet::new)
            .insert(tbl_id);
    }

    // go: Builder.initMisc L1
    pub(crate) fn init_misc(
        &mut self,
        policies: Vec<PolicyInfo>,
        resource_groups: Vec<ResourceGroupInfo>,
    ) {
        let info = Arc::make_mut(&mut self.is.misc);
        // build the policies.
        for policy in policies {
            info.set_policy(Arc::new(policy));
        }

        // build the groups.
        for group in resource_groups {
            info.set_resource_group(Arc::new(group));
        }
    }
}
