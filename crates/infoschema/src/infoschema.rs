//! go-file: pkg/infoschema/infoschema.go
//!
//! The parts of infoschema.go that infoschema v2 uses: `infoSchemaMisc` (schema version,
//! placement policies and bundles, resource groups, temporary table IDs) and the helpers.
//!
//! Infoschema v1 (`infoSchema`'s `schemaMap` / `schemaID2Name` / `sortedTablesBuckets` /
//! `referredForeignKeyMap` and the methods that read them, `MockInfoSchema*`) is not
//! ported: owner decision, only v2 matters (spec §1.2). Under v2 Go keeps those maps empty,
//! so the calls the v2 code makes into them (`b.infoSchema.deleteReferredForeignKeys`,
//! the bucket sort in `applyCreateTable`) do nothing and are left out.
//!
//! Part 2 (spec §1.3): the `InfoSchema` interface, `SessionTables`,
//! `SessionExtendedInfoSchema`, and the helpers taking the interface (`SchemaByTable`,
//! `TableIsView`, `TableIsSequence`, `GetSequenceByName`, `FindTableByTblOrPartID`,
//! `AllSchemaNames`), `init` (with D06.6's memory tables).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tidb_base::mysql;
use tidb_model::{DBInfo, PolicyInfo, ResourceGroupInfo, TableInfo};
use tidb_parser::model::CIStr;

use crate::placement::Bundle;
use crate::table::TableRef;

/// The tables of one schema by lower-case name (Go `schemaTables`). Infoschema v2 keeps it
/// only for the memory schemas (`Data.specials`).
pub struct SchemaTables {
    pub db_info: Arc<DBInfo>,
    pub tables: HashMap<String, TableRef>,
}

/// The InfoSchema data that is not the schemas and tables (Go `infoSchemaMisc`).
///
/// Go guards the policy and resource group maps with mutexes. Only the builder that owns an
/// `InfoSchemaMisc` writes it (before `Build`), so it needs no lock here (L2).
#[derive(Clone, Default)]
pub struct InfoSchemaMisc {
    /// schemaMetaVersion is the version of schema, and we should check version when change
    /// schema.
    pub(crate) schema_meta_version: i64,
    /// ruleBundleMap stores all placement rules
    pub(crate) rule_bundle_map: HashMap<i64, Arc<Bundle>>,
    /// policyMap stores all placement policies.
    pub(crate) policy_map: HashMap<String, Arc<PolicyInfo>>,
    /// resourceGroupMap stores all resource groups.
    pub(crate) resource_group_map: HashMap<String, Arc<ResourceGroupInfo>>,
    /// temporaryTables stores the temporary table ids (`None`: Go's nil map)
    pub(crate) temporary_table_ids: Option<HashSet<i64>>,
}

/// newInfoSchema: the misc part (the v1 maps are not ported).
// go: newInfoSchema L2 (only infoSchemaMisc: the v1 maps are not ported, spec §1.2)
pub fn new_info_schema() -> InfoSchemaMisc {
    InfoSchemaMisc {
        schema_meta_version: 0,
        rule_bundle_map: HashMap::new(),
        policy_map: HashMap::new(),
        resource_group_map: HashMap::new(),
        temporary_table_ids: None,
    }
}

impl InfoSchemaMisc {
    // go: infoSchema.PolicyByID L1 [M-MAPORDER]
    pub fn policy_by_id(&self, id: i64) -> Option<Arc<PolicyInfo>> {
        // TODO: use another hash map to avoid traveling on the policy map
        self.policy_map.values().find(|v| v.id == id).cloned()
    }

    /// HasTemporaryTable returns whether information schema has temporary table
    // go: infoSchemaMisc.HasTemporaryTable L1
    pub fn has_temporary_table(&self) -> bool {
        self.temporary_table_ids
            .as_ref()
            .is_some_and(|ids| !ids.is_empty())
    }

    // go: infoSchemaMisc.SchemaMetaVersion L1
    pub fn schema_meta_version(&self) -> i64 {
        self.schema_meta_version
    }

    /// PolicyByName is used to find the policy.
    // go: infoSchemaMisc.PolicyByName L2 (no lock: only the owning builder writes)
    pub fn policy_by_name(&self, name: &CIStr) -> Option<Arc<PolicyInfo>> {
        self.policy_map.get(&name.l).cloned()
    }

    /// ResourceGroupByName is used to find the resource group.
    // go: infoSchemaMisc.ResourceGroupByName L2 (no lock: only the owning builder writes)
    pub fn resource_group_by_name(&self, name: &CIStr) -> Option<Arc<ResourceGroupInfo>> {
        self.resource_group_map.get(&name.l).cloned()
    }

    /// ResourceGroupByID is used to find the resource group.
    // go: infoSchemaMisc.ResourceGroupByID L2 (no lock: only the owning builder writes; map order as Go, M-MAPORDER)
    pub fn resource_group_by_id(&self, id: i64) -> Option<Arc<ResourceGroupInfo>> {
        self.resource_group_map
            .values()
            .find(|v| v.id == id)
            .cloned()
    }

    /// AllResourceGroups returns all resource groups.
    // go: infoSchemaMisc.AllResourceGroups L2 (no lock: only the owning builder writes; map order as Go, M-MAPORDER)
    pub fn all_resource_groups(&self) -> Vec<Arc<ResourceGroupInfo>> {
        self.resource_group_map.values().cloned().collect()
    }

    // go: infoSchemaMisc.CloneResourceGroups L2 (no lock: only the owning builder writes)
    pub fn clone_resource_groups(&self) -> HashMap<String, Arc<ResourceGroupInfo>> {
        self.resource_group_map.clone()
    }

    /// AllPlacementPolicies returns all placement policies
    // go: infoSchemaMisc.AllPlacementPolicies L2 (no lock: only the owning builder writes; map order as Go, M-MAPORDER)
    pub fn all_placement_policies(&self) -> Vec<Arc<PolicyInfo>> {
        self.policy_map.values().cloned().collect()
    }

    // go: infoSchemaMisc.ClonePlacementPolicies L2 (no lock: only the owning builder writes)
    pub fn clone_placement_policies(&self) -> HashMap<String, Arc<PolicyInfo>> {
        self.policy_map.clone()
    }

    // go: infoSchemaMisc.PlacementBundleByPhysicalTableID L1
    pub fn placement_bundle_by_physical_table_id(&self, id: i64) -> Option<Arc<Bundle>> {
        self.rule_bundle_map.get(&id).cloned()
    }

    // go: infoSchemaMisc.AllPlacementBundles L1 [M-MAPORDER]
    pub fn all_placement_bundles(&self) -> Vec<Arc<Bundle>> {
        self.rule_bundle_map.values().cloned().collect()
    }

    // go: infoSchemaMisc.setResourceGroup L2 (no lock: only the owning builder writes)
    pub fn set_resource_group(&mut self, resource_group: Arc<ResourceGroupInfo>) {
        self.resource_group_map
            .insert(resource_group.name.l.clone(), resource_group);
    }

    // go: infoSchemaMisc.deleteResourceGroup L2 (no lock: only the owning builder writes)
    pub fn delete_resource_group(&mut self, name: &str) {
        self.resource_group_map.remove(name);
    }

    // go: infoSchemaMisc.setPolicy L2 (no lock: only the owning builder writes)
    pub fn set_policy(&mut self, policy: Arc<PolicyInfo>) {
        self.policy_map.insert(policy.name.l.clone(), policy);
    }

    // go: infoSchemaMisc.deletePolicy L2 (no lock: only the owning builder writes)
    pub fn delete_policy(&mut self, name: &str) {
        self.policy_map.remove(name);
    }
}

/// HasAutoIncrementColumn checks whether the table has auto_increment columns, if so,
/// return true and the column name.
// go: HasAutoIncrementColumn L1
pub fn has_auto_increment_column(tb_info: &TableInfo) -> (bool, String) {
    for col in tb_info.columns.iter().flatten() {
        if mysql::has_auto_increment_flag(col.get_flag()) {
            return (true, col.name.l.clone());
        }
    }
    (false, String::new())
}

// go: getTableInfo L1
pub(crate) fn get_table_info(tbl: Option<&TableRef>) -> Option<Arc<TableInfo>> {
    tbl.map(|t| Arc::clone(t.meta()))
}

/// `None` for Go's nil slice.
// go: getTableInfoList L1
pub(crate) fn get_table_info_list(tables: Option<&[TableRef]>) -> Option<Vec<Arc<TableInfo>>> {
    let tables = tables?;
    let mut info_lost = Vec::with_capacity(tables.len());
    for tbl in tables {
        info_lost.push(Arc::clone(tbl.meta()));
    }
    Some(info_lost)
}
