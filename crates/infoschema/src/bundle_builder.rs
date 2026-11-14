//! go-file: pkg/infoschema/bundle_builder.go
//!
//! The placement bundles of the tables (`ruleBundleMap`). The bundles themselves are built
//! by `pkg/ddl/placement` (X, [`crate::placement`]). The v1 paths
//! (`updateInfoSchemaBundles`, `completeUpdateTables`) are not ported (spec §1.2); the v2
//! ones are in infoschema_v2.rs as in Go.

use std::collections::HashMap;
use std::sync::Arc;

use tidb_base::Error;
use tidb_base::gofmt::Arg;
use tidb_model::PolicyInfo;

use crate::infoschema::InfoSchemaMisc;
use crate::infoschema_v2::{Ctx, InfoSchemaV2};
use crate::placement::{self, PolicyGetter};

struct PolicyGetterImpl<'a> {
    is: &'a InfoSchemaMisc,
}

impl PolicyGetter for PolicyGetterImpl<'_> {
    // go: policyGetter.GetPolicy L1
    fn get_policy(&self, policy_id: i64) -> Result<Arc<PolicyInfo>, Error> {
        if let Some(policy) = self.is.policy_by_id(policy_id) {
            return Ok(policy);
        }
        Err(tidb_base::errors::errorf(
            "Cannot find placement policy with ID: %d",
            &[Arg::Int(policy_id, "int64")],
        ))
    }
}

/// The placement bundles to update when the builder finishes.
#[derive(Default)]
pub struct BundleInfoBuilder {
    pub(crate) delta_update: bool,
    /// tables or partitions that need to update placement bundle
    pub(crate) update_tables: HashMap<i64, ()>,
    /// all tables or partitions referring these policies should update placement bundle
    pub(crate) update_policies: HashMap<i64, ()>,
}

impl BundleInfoBuilder {
    // go: bundleInfoBuilder.initBundleInfoBuilder L1
    pub(crate) fn init_bundle_info_builder(&mut self) {
        self.update_tables = HashMap::new();
        self.update_policies = HashMap::new();
    }

    // go: bundleInfoBuilder.SetDeltaUpdateBundles L1
    pub fn set_delta_update_bundles(&mut self) {
        self.delta_update = true;
    }

    // go: bundleInfoBuilder.deleteBundle L1
    pub(crate) fn delete_bundle(&mut self, is: &mut InfoSchemaMisc, tbl_id: i64) {
        is.rule_bundle_map.remove(&tbl_id);
    }

    // go: bundleInfoBuilder.markTableBundleShouldUpdate L1
    pub(crate) fn mark_table_bundle_should_update(&mut self, tbl_id: i64) {
        self.update_tables.insert(tbl_id, ());
    }

    // go: bundleInfoBuilder.markBundlesReferPolicyShouldUpdate L1
    pub(crate) fn mark_bundles_refer_policy_should_update(&mut self, policy_id: i64) {
        self.update_policies.insert(policy_id, ());
    }

    /// `is` is the InfoSchema being built (Go's `infoSchemaInterface`), `misc` its
    /// `base()` that receives the bundles.
    // go: bundleInfoBuilder.updateTableBundles L1
    pub(crate) async fn update_table_bundles(
        &mut self,
        is: &InfoSchemaV2,
        misc: &mut InfoSchemaMisc,
        table_id: i64,
    ) {
        let Some(tbl) = is.table_by_id(Ctx::background(), table_id).await else {
            self.delete_bundle(misc, table_id);
            return;
        };

        let bundle = placement::new_table_bundle(&PolicyGetterImpl { is: misc }, tbl.meta());
        match bundle {
            Err(err) => log::error!("create table bundle failed, error: {err}"),
            Ok(Some(bundle)) => {
                misc.rule_bundle_map.insert(table_id, Arc::new(bundle));
            }
            Ok(None) => self.delete_bundle(misc, table_id),
        }

        let Some(partition) = &tbl.meta().partition else {
            return;
        };

        for par in partition.definitions.iter().flatten() {
            let bundle = placement::new_partition_bundle(&PolicyGetterImpl { is: misc }, par);
            match bundle {
                Err(err) => log::error!(
                    "create partition bundle failed, error: {err}, partition id: {}",
                    par.id
                ),
                Ok(Some(bundle)) => {
                    misc.rule_bundle_map.insert(par.id, Arc::new(bundle));
                }
                Ok(None) => self.delete_bundle(misc, par.id),
            }
        }
    }
}
