//! go-file: pkg/infoschema/context/infoschema.go
//!
//! Package `infoschema/context`: the special-attribute filters of
//! `ListTablesWithSpecialAttribute`, and `DBInfoAsInfoSchema`. The `MetaOnlyInfoSchema` /
//! `Misc` interfaces are used below this crate (the expression context reads the
//! InfoSchema), so their stand-in trait lives in tidb-sessionctx
//! (`expropt::MetaOnlyInfoSchema`); [`crate::InfoSchemaV2`] implements it.
//!
//! Go's filters are package variables holding closures; here they are functions of type
//! [`SpecialAttributeFilter`].

#![allow(non_upper_case_globals)]

use std::sync::Arc;

use tidb_base::Error;
use tidb_model::{DBInfo, StatePublic, TableInfo};
use tidb_parser::model::CIStr;

/// SpecialAttributeFilter is used to filter tables with special attributes.
pub type SpecialAttributeFilter = fn(&TableInfo) -> bool;

/// TTLAttribute is the TTL attribute filter used by ListTablesWithSpecialAttribute.
// glue: Go package variable TTLAttribute (a closure)
pub fn ttl_attribute(t: &TableInfo) -> bool {
    t.state == StatePublic && t.ttl_info.is_some()
}

/// TiFlashAttribute is the TiFlashReplica attribute filter used by
/// ListTablesWithSpecialAttribute.
// glue: Go package variable TiFlashAttribute (a closure)
pub fn tiflash_attribute(t: &TableInfo) -> bool {
    t.tiflash_replica.is_some()
}

/// PlacementPolicyAttribute is the Placement Policy attribute filter used by
/// ListTablesWithSpecialAttribute.
// glue: Go package variable PlacementPolicyAttribute (a closure)
pub fn placement_policy_attribute(t: &TableInfo) -> bool {
    if t.placement_policy_ref.is_some() {
        return true;
    }
    if let Some(par_info) = t.get_partition_info() {
        for def in par_info.definitions.iter().flatten() {
            if def.placement_policy_ref.is_some() {
                return true;
            }
        }
    }
    false
}

/// AllPlacementPolicyAttribute is the Placement Policy attribute filter used by
/// ListTablesWithSpecialAttribute. Different from PlacementPolicyAttribute,
/// Partition.Enable flag will be ignored.
// glue: Go package variable AllPlacementPolicyAttribute (a closure)
pub fn all_placement_policy_attribute(t: &TableInfo) -> bool {
    if t.placement_policy_ref.is_some() {
        return true;
    }
    if let Some(p) = &t.partition {
        for def in p.definitions.iter().flatten() {
            if def.placement_policy_ref.is_some() {
                return true;
            }
        }
    }
    false
}

/// TableLockAttribute is the Table Lock attribute filter used by
/// ListTablesWithSpecialAttribute.
// glue: Go package variable TableLockAttribute (a closure)
pub fn table_lock_attribute(t: &TableInfo) -> bool {
    t.lock.is_some()
}

/// PartitionAttribute is the Partition attribute filter used by
/// ListTablesWithSpecialAttribute.
// glue: Go package variable PartitionAttribute (a closure)
pub fn partition_attribute(t: &TableInfo) -> bool {
    t.get_partition_info().is_some()
}

/// AffinityAttribute is the Affinity attribute filter used by
/// ListTablesWithSpecialAttribute.
// glue: Go package variable AffinityAttribute (a closure)
pub fn affinity_attribute(t: &TableInfo) -> bool {
    t.affinity.is_some()
}

/// HasSpecialAttributes checks if a table has any special attributes.
// go: HasSpecialAttributes L1
pub fn has_special_attributes(t: &TableInfo) -> bool {
    ttl_attribute(t)
        || tiflash_attribute(t)
        || placement_policy_attribute(t)
        || partition_attribute(t)
        || table_lock_attribute(t)
        || affinity_attribute(t)
}

/// AllSpecialAttribute marks a model.TableInfo with any special attributes.
pub const AllSpecialAttribute: SpecialAttributeFilter = has_special_attributes;

/// TableInfoResult is used to store the result of ListTablesWithSpecialAttribute.
#[derive(Clone, Debug, Default)]
pub struct TableInfoResult {
    pub db_name: CIStr,
    pub table_infos: Vec<Arc<TableInfo>>,
}

/// DBInfoAsInfoSchema is used mainly in test.
pub struct DBInfoAsInfoSchema(pub Vec<DBInfo>);

impl DBInfoAsInfoSchema {
    /// AllSchemas implement infoschema.SchemaAndTable interface.
    // go: DBInfoAsInfoSchema.AllSchemas L1
    pub fn all_schemas(&self) -> &[DBInfo] {
        &self.0
    }

    /// SchemaTableInfos implement infoschema.SchemaAndTable interface. Go compares the
    /// whole `CIStr` (`db.Name == schema`); a missing schema gives no tables.
    // go: DBInfoAsInfoSchema.SchemaTableInfos L1
    pub fn schema_table_infos(&self, schema: &CIStr) -> Result<&[TableInfo], Error> {
        for db in &self.0 {
            if db.name == *schema {
                return Ok(db.deprecated.tables.as_deref().unwrap_or(&[]));
            }
        }
        Ok(&[])
    }
}
