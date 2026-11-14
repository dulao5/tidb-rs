//! go-file: pkg/infoschema/builder.go
//!
//! The InfoSchema builder: a full load (`InitWithDBInfos`) or the schema diffs applied on
//! top of the previous version (`InitWithOldInfoSchema` + `ApplyDiff`).
//!
//! Only the v2 paths are ported (spec §1.2): `Builder.enableV2` is always true, so the v1
//! branches and the v1-only methods (`applyTableUpdate`, `applyDropSchema`,
//! `applyRecoverSchema`, `applyDropTable`, `applyModifySchema*`, `copySortedTables*`,
//! `getSchemaAndCopyIfNecessary`, `deleteReferredForeignKeys`, `sortAllTablesByID`,
//! `tableBucketIdx`) are not; the bucket sort that the v2 path of `applyCreateTable` runs
//! on an empty v1 bucket is left out.
//!
//! Not ported here:
//! - `updateAutoIDForExchangePartition`: a meta write (X, "DDL 与元数据写入由 Go TiDB
//!   承担"). Every Go TiDB applying the diff writes the same values, so the cluster's auto
//!   IDs do not depend on tidb-rs;
//! - the failpoint `repairFetchCreateTable` and the metric `DDLResetTempIndexWrite`.

#![allow(non_upper_case_globals)]

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use futures::FutureExt;
use futures::future::BoxFuture;
use parking_lot::RwLock;
use tidb_base::Error;
use tidb_base::gofmt::Arg;
use tidb_base::gostd::to_lower;
use tidb_kv::kv::Storage;
use tidb_model::{
    ActionAddTablePartition, ActionAlterPlacementPolicy, ActionAlterResourceGroup,
    ActionAlterSequence, ActionAlterTablePartitionPlacement, ActionAlterTablePartitioning,
    ActionAlterTablePlacement, ActionCreatePlacementPolicy, ActionCreateResourceGroup,
    ActionCreateSchema, ActionCreateSequence, ActionCreateTable, ActionCreateTables,
    ActionCreateView, ActionDropPlacementPolicy, ActionDropResourceGroup, ActionDropSchema,
    ActionDropSequence, ActionDropTable, ActionDropTablePartition, ActionDropView,
    ActionExchangeTablePartition, ActionFlashbackCluster, ActionMap, ActionModifyColumn,
    ActionModifySchemaCharsetAndCollate, ActionModifySchemaDefaultPlacement,
    ActionModifyTableAutoIDCache, ActionMultiSchemaChange, ActionRebaseAutoID,
    ActionRebaseAutoRandomBase, ActionRecoverSchema, ActionRecoverTable, ActionRefreshMeta,
    ActionRemovePartitioning, ActionRenameTable, ActionRenameTables, ActionReorganizePartition,
    ActionRepairTable, ActionTruncateTable, ActionTruncateTablePartition, ActionType,
    AffectedOption, ColumnInfoVersion2, DBInfo, GoCopy, PolicyInfo, PolicyRefInfo,
    ResourceGroupInfo, SchemaDiff, StateNone, StatePublic, TableInfo, TableInfoVersion2,
    TableInfoVersion3, TempTableNone,
};
use tidb_parser::gocharset::{CharsetUTF8, CharsetUTF8MB4, CollationUTF8MB4};
use tidb_parser::model::CIStr;

use crate::autoid::{
    self, Allocator, AllocatorType, Allocators, AutoIncrementType, AutoRandomType, Requirement,
    RowIDAllocType,
};
use crate::builder_misc::{
    apply_alter_policy, apply_create_or_alter_resource_group, apply_create_policy,
    apply_drop_policy, apply_drop_resource_group,
};
use crate::bundle_builder::BundleInfoBuilder;
use crate::error::{ErrDatabaseNotExists, ErrTableNotExists};
use crate::infoschema::SchemaTables;
use crate::infoschema_v2::{
    Data, InfoSchemaV2, TableItem, alloc_by_id, apply_create_schema, apply_create_tables,
    apply_drop_schema, apply_drop_table, apply_modify_schema_charset_and_collate,
    apply_modify_schema_default_placement, apply_recover_schema, apply_table_update, is_special_db,
    new_info_schema_v2, old_schema_info, schema_cache_capacity, update_info_schema_bundles,
};
use crate::meta::Mutator;
use crate::table::{self, TableRef};

/// The session factory of cached tables (Go `func() (pools.Resource, error)`).
pub type Factory = Arc<dyn Fn() -> Result<Box<dyn std::any::Any + Send>, Error> + Send + Sync>;

/// Builder builds a new InfoSchema.
pub struct Builder {
    /// Go embeds `infoschemaV2`; its `infoSchemaMisc` is the misc being built.
    pub(crate) is: InfoSchemaV2,

    /// Used by autoid allocators
    pub(crate) requirement: Option<Arc<dyn Requirement>>,

    pub(crate) factory: Option<Factory>,
    pub(crate) bundle: BundleInfoBuilder,
    pub(crate) info_data: Arc<Data>,
    pub(crate) store: Option<Arc<dyn Storage>>,

    /// N11 §4.3 (draft, D7 pending): refuse a schema diff whose type v8.5.8 does not know.
    refuse_unknown_actions: bool,
}

impl Builder {
    /// ApplyDiff applies SchemaDiff to the new InfoSchema.
    /// Return the detail updated table IDs that are produced from SchemaDiff and an error.
    // go: Builder.ApplyDiff L1
    pub fn apply_diff<'a>(
        &'a mut self,
        m: &'a Mutator,
        diff: &'a SchemaDiff,
    ) -> BoxFuture<'a, Result<Vec<i64>, Error>> {
        async move {
            Arc::make_mut(&mut self.is.misc).schema_meta_version = diff.version;
            match diff.type_ {
                ActionCreateSchema => apply_create_schema(self, m, diff)
                    .await
                    .map(|()| Vec::new()),
                ActionDropSchema => Ok(apply_drop_schema(self, diff).await),
                ActionRecoverSchema => apply_recover_schema(self, m, diff).await,
                ActionModifySchemaCharsetAndCollate => {
                    apply_modify_schema_charset_and_collate(self, m, diff)
                        .await
                        .map(|()| Vec::new())
                }
                ActionModifySchemaDefaultPlacement => {
                    apply_modify_schema_default_placement(self, m, diff)
                        .await
                        .map(|()| Vec::new())
                }
                ActionCreatePlacementPolicy => apply_create_policy(self, m, diff)
                    .await
                    .map(|()| Vec::new()),
                ActionDropPlacementPolicy => Ok(apply_drop_policy(self, diff.schema_id)),
                ActionAlterPlacementPolicy => apply_alter_policy(self, m, diff).await,
                ActionCreateResourceGroup => apply_create_or_alter_resource_group(self, m, diff)
                    .await
                    .map(|()| Vec::new()),
                ActionAlterResourceGroup => apply_create_or_alter_resource_group(self, m, diff)
                    .await
                    .map(|()| Vec::new()),
                ActionDropResourceGroup => Ok(apply_drop_resource_group(self, m, diff)),
                ActionTruncateTablePartition | ActionTruncateTable => {
                    apply_truncate_table_or_partition(self, m, diff).await
                }
                ActionDropTable | ActionDropTablePartition => {
                    apply_drop_table_or_partition(self, m, diff).await
                }
                ActionRecoverTable => apply_recover_table(self, m, diff).await,
                ActionCreateTables => apply_create_tables(self, m, diff).await,
                ActionReorganizePartition
                | ActionRemovePartitioning
                | ActionAlterTablePartitioning => apply_reorganize_partition(self, m, diff).await,
                ActionExchangeTablePartition => apply_exchange_table_partition(self, m, diff).await,
                ActionFlashbackCluster => Ok(vec![-1]),
                ActionRefreshMeta => apply_refresh_meta(self, m, diff).await,
                _ => {
                    if self.refuse_unknown_actions && !is_known_action_type(diff.type_) {
                        return Err(unknown_action_error(diff.type_));
                    }
                    apply_default_action(self, m, diff).await
                }
            }
        }
        .boxed()
    }

    /// N11 §4.3 (draft): with `refuse`, `ApplyDiff` returns an error instead of Go's
    /// default branch for a diff type v8.5.8 does not know, so the domain can stop serving
    /// (an upgraded cluster ran a DDL tidb-rs cannot interpret). Off by default (Go's
    /// behaviour) until the owner decides N11 D7.
    // glue: N11 §4.3 upgrade check (draft)
    pub fn set_refuse_unknown_actions(&mut self, refuse: bool) {
        self.refuse_unknown_actions = refuse;
    }

    // go: Builder.applyCreateTables L1
    pub(crate) async fn apply_create_tables(
        &mut self,
        m: &Mutator,
        diff: &SchemaDiff,
    ) -> Result<Vec<i64>, Error> {
        let n = diff.affected_opts.as_ref().map_or(0, Vec::len);
        self.apply_affected_opts(m, Vec::with_capacity(n), diff, ActionCreateTable)
            .await
    }
}

/// Whether v8.5.8 knows the action type (`model.ActionMap`).
// glue: N11 §4.3 upgrade check (draft)
pub fn is_known_action_type(tp: ActionType) -> bool {
    ActionMap.iter().any(|(a, _)| *a == tp)
}

/// The error of an unknown diff type (N11 D7 recommends 1105 with an explicit text).
// glue: N11 §4.3 upgrade check (draft)
fn unknown_action_error(tp: ActionType) -> Error {
    tidb_base::errors::errorf(
        "tidb-rs v8.5.8 cannot read metadata written by a newer TiDB: unknown schema diff type %d",
        &[Arg::Int(i64::from(tp.0), "model.ActionType")],
    )
}

/// equalPlacementPolicy compares two placement policy references for equality
// go: equalPlacementPolicy L1
fn equal_placement_policy(a: Option<&PolicyRefInfo>, b: Option<&PolicyRefInfo>) -> bool {
    match (a, b) {
        (None, None) => true,
        (None, _) | (_, None) => false,
        (Some(a), Some(b)) => a.id == b.id && a.name.l == b.name.l,
    }
}

/// A schema diff built inside the builder (Go's `&model.SchemaDiff{...}` literals).
// glue: Go composite literal of model.SchemaDiff
fn new_diff(
    version: i64,
    tp: ActionType,
    schema_id: i64,
    table_id: i64,
    old_schema_id: i64,
    old_table_id: i64,
) -> SchemaDiff {
    SchemaDiff {
        version,
        type_: tp,
        schema_id,
        table_id,
        old_schema_id,
        old_table_id,
        ..Default::default()
    }
}

/// applyRefreshMeta handles schema and table operations during PITR restore.
///
/// IMPORTANT: This function relies on BR (Backup & Restore) to send operations in a
/// specific sequence:
/// 1. Delete tables first
/// 2. Delete schemas second
/// 3. Create/Update new schemas third
/// 4. Create/Update new tables fourth
///
/// This sequence is critical because:
///   - If schemas are deleted before their tables, the table deletion operations will
///     fail since the schema context is needed to properly clean up table metadata
///   - Schema creation must happen before table creation to provide the proper context
// go: applyRefreshMeta L1
async fn apply_refresh_meta(
    b: &mut Builder,
    m: &Mutator,
    diff: &SchemaDiff,
) -> Result<Vec<i64>, Error> {
    let schema_id = diff.schema_id;
    let table_id = diff.table_id;
    let old_schema_id = diff.schema_id;
    let old_table_id = diff.table_id;

    // Schema operation
    if table_id == 0 {
        let (db_info, err) = m.get_database(schema_id).await;
        err?;

        // Schema doesn't exist in kv, drop it from infoschema.
        let Some(db_info) = db_info else {
            let mut schema_diff = new_diff(
                diff.version,
                ActionDropSchema,
                schema_id,
                0,
                old_schema_id,
                old_table_id,
            );
            schema_diff.is_refresh_meta = true;
            return Ok(apply_drop_schema(b, &schema_diff).await);
        };

        // Schema exists in kv but not in infoschema, create it to infoschema
        match b.schema_by_id(schema_id) {
            None => {
                let mut schema_diff = new_diff(
                    diff.version,
                    ActionCreateSchema,
                    schema_id,
                    0,
                    old_schema_id,
                    old_table_id,
                );
                schema_diff.is_refresh_meta = true;
                apply_create_schema(b, m, &schema_diff).await?;
            }
            Some(current_schema) => {
                let needs_charset_update = current_schema.charset != db_info.charset
                    || current_schema.collate != db_info.collate;
                let needs_placement_update = !equal_placement_policy(
                    current_schema.placement_policy_ref.as_deref(),
                    db_info.placement_policy_ref.as_deref(),
                );

                if needs_charset_update {
                    let mut charset_diff = new_diff(
                        diff.version,
                        ActionModifySchemaCharsetAndCollate,
                        schema_id,
                        0,
                        old_schema_id,
                        old_table_id,
                    );
                    charset_diff.is_refresh_meta = true;
                    apply_modify_schema_charset_and_collate(b, m, &charset_diff).await?;
                }

                if needs_placement_update {
                    let mut placement_diff = new_diff(
                        diff.version,
                        ActionModifySchemaDefaultPlacement,
                        schema_id,
                        0,
                        old_schema_id,
                        old_table_id,
                    );
                    placement_diff.is_refresh_meta = true;
                    apply_modify_schema_default_placement(b, m, &placement_diff).await?;
                }
            }
        }
        return Ok(Vec::new());
    }

    // Table operation - check if database exists in infoschema first
    // if not just return, since infoschema is consistent, if schema is gone then all tables must have gone.
    if b.schema_by_id(schema_id).is_none() {
        return Ok(Vec::new());
    }

    // Check if table exists in kv
    let (table_info, err) = m.get_table(schema_id, table_id).await;
    if let Err(err) = err
        && !crate::meta::ErrDBNotExists.equal(&err)
    {
        return Err(err);
    }

    // Table not exists in kv, drop it from infoschema
    if table_info.is_none() {
        let mut schema_diff = new_diff(
            diff.version,
            ActionDropTable,
            schema_id,
            table_id,
            old_schema_id,
            old_table_id,
        );
        schema_diff.is_refresh_meta = true;
        return apply_drop_table_or_partition(b, m, &schema_diff).await;
    }
    // default update table
    let mut schema_diff = new_diff(
        diff.version,
        ActionCreateTable,
        schema_id,
        table_id,
        old_schema_id,
        old_table_id,
    );
    schema_diff.is_refresh_meta = true;
    apply_default_action(b, m, &schema_diff).await
}

// go: applyTruncateTableOrPartition L1
async fn apply_truncate_table_or_partition(
    b: &mut Builder,
    m: &Mutator,
    diff: &SchemaDiff,
) -> Result<Vec<i64>, Error> {
    let mut tbl_ids = apply_table_update(b, m, diff).await?;

    // bundle ops
    if diff.type_ == ActionTruncateTable {
        b.delete_bundle(diff.old_table_id);
    }
    b.bundle.mark_table_bundle_should_update(diff.table_id);
    // TODO: check that all partitions are updated to the cache!

    for opt in diff.affected_opts.iter().flatten() {
        if diff.type_ == ActionTruncateTablePartition {
            // Reduce the impact on DML when executing partition DDL. eg.
            // While session 1 performs the DML operation associated with partition 1,
            // the TRUNCATE operation of session 2 on partition 2 does not cause the operation of session 1 to fail.
            tbl_ids.push(opt.old_table_id);
        }
        b.delete_bundle(opt.old_table_id);
    }
    Ok(tbl_ids)
}

// go: applyDropTableOrPartition L1
async fn apply_drop_table_or_partition(
    b: &mut Builder,
    m: &Mutator,
    diff: &SchemaDiff,
) -> Result<Vec<i64>, Error> {
    let tbl_ids = apply_table_update(b, m, diff).await?;

    // bundle ops
    b.bundle.mark_table_bundle_should_update(diff.table_id);
    for opt in diff.affected_opts.iter().flatten() {
        b.delete_bundle(opt.old_table_id);
    }
    Ok(tbl_ids)
}

// go: applyReorganizePartition L1
async fn apply_reorganize_partition(
    b: &mut Builder,
    m: &Mutator,
    diff: &SchemaDiff,
) -> Result<Vec<i64>, Error> {
    let tbl_ids = apply_table_update(b, m, diff).await?;

    // The table might have changed TableID
    if diff.table_id != diff.old_table_id && diff.old_table_id != 0 {
        b.delete_bundle(diff.old_table_id);
    }
    b.bundle.mark_table_bundle_should_update(diff.table_id);

    // bundle ops
    for opt in diff.affected_opts.iter().flatten() {
        if opt.old_table_id != 0 {
            b.delete_bundle(opt.old_table_id);
        }
    }
    Ok(tbl_ids)
}

/// Go ends with `updateAutoIDForExchangePartition`, a meta write that tidb-rs leaves to
/// Go TiDB (X, module doc).
// go: applyExchangeTablePartition L2 (the auto ID write updateAutoIDForExchangePartition is X)
async fn apply_exchange_table_partition(
    b: &mut Builder,
    m: &Mutator,
    diff: &SchemaDiff,
) -> Result<Vec<i64>, Error> {
    // It is not in StatePublic.
    if diff.old_table_id == diff.table_id && diff.old_schema_id == diff.schema_id {
        let nt_ids = apply_table_update(b, m, diff).await?;
        let Some(first) = diff.affected_opts.as_ref().and_then(|o| o.first().cloned()) else {
            return Ok(nt_ids);
        };
        if first.old_schema_id == 0 {
            return Ok(nt_ids);
        }
        // Reload parition tabe.
        let pt_schema_id = first.old_schema_id;
        let pt_id = first.table_id;
        let pt_diff = new_diff(
            diff.version,
            diff.type_,
            pt_schema_id,
            pt_id,
            pt_schema_id,
            pt_id,
        );
        let mut pt_ids = apply_table_update(b, m, &pt_diff).await?;
        pt_ids.extend(nt_ids);
        return Ok(pt_ids);
    }
    let nt_schema_id = diff.old_schema_id;
    let nt_id = diff.old_table_id;
    let mut pt_schema_id = diff.schema_id;
    let mut pt_id = diff.table_id;
    let part_id = diff.table_id;
    if let Some(first) = diff.affected_opts.as_ref().and_then(|o| o.first()) {
        pt_id = first.table_id;
        if first.schema_id != 0 {
            pt_schema_id = first.schema_id;
        }
    }
    // The normal table needs to be updated first:
    // Just update the tables separately
    let mut curr_diff = new_diff(diff.version, diff.type_, nt_schema_id, nt_id, 0, 0);
    // This is only for the case since https://github.com/pingcap/tidb/pull/45877
    // Fixed now, by adding back the AffectedOpts
    // to carry the partitioned Table ID.
    if pt_id != part_id {
        curr_diff.table_id = part_id;
        curr_diff.old_table_id = nt_id;
        curr_diff.old_schema_id = nt_schema_id;
    }
    let nt_ids = apply_table_update(b, m, &curr_diff).await?;
    // partID is the new id for the non-partitioned table!
    b.bundle.mark_table_bundle_should_update(part_id);
    b.bundle.mark_table_bundle_should_update(pt_id);
    // Then the partitioned table, will re-read the whole table, including all partitions!
    curr_diff.table_id = pt_id;
    curr_diff.schema_id = pt_schema_id;
    curr_diff.old_table_id = pt_id;
    curr_diff.old_schema_id = pt_schema_id;
    let mut pt_ids = apply_table_update(b, m, &curr_diff).await?;
    // ntID is the new id for the partition!
    // (updateAutoIDForExchangePartition(store, ptSchemaID, ptID, ntSchemaID, ntID): X)
    pt_ids.extend(nt_ids);
    Ok(pt_ids)
}

// go: applyRecoverTable L1
async fn apply_recover_table(
    b: &mut Builder,
    m: &Mutator,
    diff: &SchemaDiff,
) -> Result<Vec<i64>, Error> {
    let tbl_ids = apply_table_update(b, m, diff).await?;

    // bundle ops
    for opt in diff.affected_opts.iter().flatten() {
        b.bundle.mark_table_bundle_should_update(opt.table_id);
    }
    Ok(tbl_ids)
}

impl Builder {
    // go: Builder.applyAffectedOpts L1
    async fn apply_affected_opts(
        &mut self,
        m: &Mutator,
        mut tbl_ids: Vec<i64>,
        diff: &SchemaDiff,
        tp: ActionType,
    ) -> Result<Vec<i64>, Error> {
        let opts: Vec<AffectedOption> = diff.affected_opts.iter().flatten().cloned().collect();
        for opt in opts {
            let affected_diff = new_diff(
                diff.version,
                tp,
                opt.schema_id,
                opt.table_id,
                opt.old_schema_id,
                opt.old_table_id,
            );
            let affected_ids = self.apply_diff(m, &affected_diff).await?;
            tbl_ids.extend(affected_ids);
        }
        Ok(tbl_ids)
    }
}

// go: applyDefaultAction L1
async fn apply_default_action(
    b: &mut Builder,
    m: &Mutator,
    diff: &SchemaDiff,
) -> Result<Vec<i64>, Error> {
    let tbl_ids = apply_table_update(b, m, diff).await?;

    b.apply_affected_opts(m, tbl_ids, diff, diff.type_).await
}

impl Builder {
    /// Returns `(oldTableID, newTableID)`.
    // go: Builder.getTableIDs L1
    pub(crate) async fn get_table_ids(
        &self,
        m: &Mutator,
        diff: &SchemaDiff,
    ) -> Result<(i64, i64), Error> {
        let mut old_table_id = 0;
        let mut new_table_id = 0;
        match diff.type_ {
            ActionCreateSequence | ActionRecoverTable => {
                new_table_id = diff.table_id;
            }
            ActionCreateTable => {
                // WARN: when support create table with foreign key in https://github.com/pingcap/tidb/pull/37148,
                // create table with foreign key requires a multi-step state change(none -> write-only -> public),
                // when the table's state changes from write-only to public, infoSchema need to drop the old table
                // which state is write-only, otherwise, infoSchema.sortedTablesBuckets will contain 2 table both
                // have the same ID, but one state is write-only, another table's state is public, it's unexpected.
                //
                // WARN: this change will break the compatibility if execute create table with foreign key DDL when upgrading TiDB,
                // since old-version TiDB doesn't know to delete the old table.
                // Since the cluster-index feature also has similar problem, we chose to prevent DDL execution during the upgrade process to avoid this issue.
                old_table_id = diff.old_table_id;
                new_table_id = diff.table_id;
            }
            ActionDropTable | ActionDropView | ActionDropSequence => {
                old_table_id = diff.table_id;
                // directly return if this action is initiated by refreshMeta DDL (only used by BR). In the BR case, we don't
                // care about ON DELETE/UPDATE CASCADE so doesn't need to go through the below logic. The most important
                // thing in BR is that we might not have DB key so if we go through, m.GetTable will error out.
                if diff.is_refresh_meta {
                    return Ok((old_table_id, new_table_id));
                }

                // Still keep the table in infoschema until when the state of table reaches StateNone. This is because
                // the `ON DELETE/UPDATE CASCADE` function of foreign key may need to find the table in infoschema. Not
                // only the table with foreign keys are stored in the infoschema to keep a consistent behavior for all
                // tables.
                let (tbl_info, err) = m.get_table(diff.schema_id, old_table_id).await;
                err?;
                if let Some(tbl_info) = tbl_info
                    && tbl_info.state != StateNone
                {
                    new_table_id = diff.table_id;
                }
            }
            ActionTruncateTable
            | ActionCreateView
            | ActionExchangeTablePartition
            | ActionAlterTablePartitioning
            | ActionRemovePartitioning => {
                old_table_id = diff.old_table_id;
                new_table_id = diff.table_id;
            }
            _ => {
                old_table_id = diff.table_id;
                new_table_id = diff.table_id;
            }
        }
        Ok((old_table_id, new_table_id))
    }

    // go: Builder.updateBundleForTableUpdate L1
    pub(crate) fn update_bundle_for_table_update(
        &mut self,
        diff: &SchemaDiff,
        new_table_id: i64,
        old_table_id: i64,
    ) {
        // handle placement rule cache
        match diff.type_ {
            ActionCreateTable | ActionAddTablePartition => {
                self.bundle.mark_table_bundle_should_update(new_table_id);
            }
            ActionDropTable => {
                self.delete_bundle(old_table_id);
            }
            ActionTruncateTable => {
                self.delete_bundle(old_table_id);
                self.bundle.mark_table_bundle_should_update(new_table_id);
            }
            ActionRecoverTable => {
                self.bundle.mark_table_bundle_should_update(new_table_id);
            }
            ActionAlterTablePlacement | ActionAlterTablePartitionPlacement => {
                self.bundle.mark_table_bundle_should_update(new_table_id);
            }
            _ => {}
        }
    }

    /// `b.deleteBundle(b.infoSchema, tblID)`.
    // glue: Go b.deleteBundle(b.infoSchema, id) (the receiver and its embedded misc)
    pub(crate) fn delete_bundle(&mut self, tbl_id: i64) {
        let misc = Arc::make_mut(&mut self.is.misc);
        self.bundle.delete_bundle(misc, tbl_id);
    }
}

// go: dropTableForUpdate L1
pub(crate) async fn drop_table_for_update(
    b: &mut Builder,
    new_table_id: i64,
    old_table_id: i64,
    db_info: &DBInfo,
    diff: &SchemaDiff,
) -> Result<(Vec<i64>, Allocators), Error> {
    let mut tbl_ids = Vec::with_capacity(2);
    let mut kept_allocs = Allocators::default();
    // We try to reuse the old allocator, so the cached auto ID can be reused.
    if table_id_is_valid(old_table_id) {
        if old_table_id == new_table_id
            // For rename table, keep the old alloc.

            // For repairing table in TiDB cluster, given 2 normal node and 1 repair node.
            // For normal node's information schema, repaired table is existed.
            // For repair node's information schema, repaired table is filtered (couldn't find it in `is`).
            // So here skip to reserve the allocators when repairing table.
            && diff.type_ != ActionRepairTable
            // Alter sequence will change the sequence info in the allocator, so the old allocator is not valid any more.
            && diff.type_ != ActionAlterSequence
            // Exchange partition updates auto IDs in meta store, so we need to reload allocators to pick up the new values.
            && diff.type_ != ActionExchangeTablePartition
        {
            // TODO: Check how this would work with ADD/REMOVE Partitioning,
            // which may have AutoID not connected to tableID
            // TODO: can there be _tidb_rowid AutoID per partition?
            let (old_allocs, _) = alloc_by_id(b, old_table_id).await;
            kept_allocs = get_kept_allocators(diff, old_allocs);
        }

        let tmp_ids =
            if (diff.type_ == ActionRenameTable || diff.type_ == ActionRenameTables)
                && diff.old_schema_id != diff.schema_id
            {
                let Some(old_db_info) = old_schema_info(b, diff) else {
                    return Err(ErrDatabaseNotExists.gen_with_stack_by_args(vec![Arg::from(
                        format!("(Schema ID {})", diff.old_schema_id),
                    )]));
                };
                apply_drop_table(b, diff, &old_db_info, old_table_id, tbl_ids.clone()).await
            } else {
                apply_drop_table(b, diff, db_info, old_table_id, tbl_ids.clone()).await
            };

        if old_table_id != new_table_id {
            // Update tblIDs only when oldTableID != newTableID because applyCreateTable() also updates tblIDs.
            tbl_ids = tmp_ids;
        }
    }
    Ok((tbl_ids, kept_allocs))
}

/// getKeptAllocators get allocators that is not changed by the DDL.
// go: getKeptAllocators L1
pub(crate) fn get_kept_allocators(diff: &SchemaDiff, old_allocs: Allocators) -> Allocators {
    let mut auto_id_changed = false;
    let mut auto_random_changed = false;
    match diff.type_ {
        ActionRebaseAutoID | ActionModifyTableAutoIDCache => auto_id_changed = true,
        ActionRebaseAutoRandomBase => auto_random_changed = true,
        ActionMultiSchemaChange => {
            for &t in diff.sub_action_types.iter().flatten() {
                match ActionType(t) {
                    ActionRebaseAutoID | ActionModifyTableAutoIDCache => auto_id_changed = true,
                    ActionRebaseAutoRandomBase => auto_random_changed = true,
                    _ => {}
                }
            }
        }
        _ => {}
    }
    if auto_id_changed {
        // Only drop auto-increment allocator.
        old_allocs.filter(|a| {
            let tp = a.get_type();
            tp != RowIDAllocType && tp != AutoIncrementType
        })
    } else if auto_random_changed {
        // Only drop auto-random allocator.
        old_allocs.filter(|a| a.get_type() != AutoRandomType)
    } else {
        // Keep all allocators.
        old_allocs
    }
}

// go: appendAffectedIDs L1
pub(crate) fn append_affected_ids(mut affected: Vec<i64>, tbl_info: &TableInfo) -> Vec<i64> {
    affected.push(tbl_info.id);
    if let Some(pi) = tbl_info.get_partition_info() {
        for def in pi.definitions.iter().flatten() {
            affected.push(def.id);
        }
    }
    affected
}

impl Builder {
    // go: Builder.applyCreateSchema L1
    pub(crate) async fn apply_create_schema(
        &mut self,
        m: &Mutator,
        diff: &SchemaDiff,
    ) -> Result<(), Error> {
        let (di, err) = m.get_database(diff.schema_id).await;
        err?;
        let Some(di) = di else {
            // When we apply an old schema diff, the database may has been dropped already, so we need to fall back to
            // full load.
            return Err(
                ErrDatabaseNotExists.gen_with_stack_by_args(vec![Arg::from(format!(
                    "(Schema ID {})",
                    diff.schema_id
                ))]),
            );
        };
        self.add_db(diff.version, di, HashMap::new());
        Ok(())
    }

    // go: Builder.buildAllocsForCreateTable L1
    fn build_allocs_for_create_table(
        &self,
        tp: ActionType,
        db_info: &DBInfo,
        tbl_info: &TableInfo,
        mut allocs: Allocators,
    ) -> Allocators {
        if !allocs.allocs.is_empty() {
            let new_alloc =
                |is_unsigned: bool, alloc_type: AllocatorType| -> Option<Arc<dyn Allocator>> {
                    autoid::new_allocator(
                        self.requirement.as_ref(),
                        db_info.id,
                        tbl_info.id,
                        is_unsigned,
                        alloc_type,
                    )
                };
            match tp {
                ActionRebaseAutoID | ActionModifyTableAutoIDCache => {
                    // If the allocator type might be AutoIncrementType, create both AutoIncrementType
                    // and RowIDAllocType allocator for it. Because auto id and row id could share the same allocator.
                    // Allocate auto id may route to allocate row id, if row id allocator is nil, the program panic!
                    for tp in [AutoIncrementType, RowIDAllocType] {
                        if let Some(a) = new_alloc(tbl_info.is_auto_inc_col_unsigned(), tp) {
                            allocs = allocs.append(a);
                        }
                    }
                }
                ActionRebaseAutoRandomBase => {
                    if let Some(a) =
                        new_alloc(tbl_info.is_auto_random_bit_col_unsigned(), AutoRandomType)
                    {
                        allocs = allocs.append(a);
                    }
                }
                // Change column attribute from auto_increment to auto_random.
                ActionModifyColumn
                    if tbl_info.contains_auto_random_bits()
                        && allocs.get(AutoRandomType).is_none() =>
                {
                    {
                        // Remove auto_increment allocator.
                        allocs = allocs.filter(|a| {
                            a.get_type() != AutoIncrementType && a.get_type() != RowIDAllocType
                        });
                        if let Some(a) =
                            new_alloc(tbl_info.is_auto_random_bit_col_unsigned(), AutoRandomType)
                        {
                            allocs = allocs.append(a);
                        }
                    }
                }
                _ => {}
            }
            return allocs;
        }
        autoid::new_allocators_from_tbl_info(self.requirement.as_ref(), db_info.id, tbl_info)
    }
}

// go: applyCreateTable L1
#[allow(clippy::too_many_arguments)]
pub(crate) async fn apply_create_table(
    b: &mut Builder,
    m: &Mutator,
    db_info: &DBInfo,
    table_id: i64,
    allocs: Allocators,
    tp: ActionType,
    mut affected: Vec<i64>,
    schema_version: i64,
) -> Result<Vec<i64>, Error> {
    let (tbl_info, err) = m.get_table(db_info.id, table_id).await;
    err?;
    let Some(mut tbl_info) = tbl_info else {
        // When we apply an old schema diff, the table may has been dropped already, so we need to fall back to
        // full load.
        return Err(ErrTableNotExists.fast_gen_by_args(vec![
            Arg::from(format!("(Schema ID {})", db_info.id)),
            Arg::from(format!("(Table ID {table_id})")),
        ]));
    };

    if tp != ActionTruncateTablePartition {
        affected = append_affected_ids(affected, &tbl_info);
    }

    convert_charset_collate_to_lower_case_if_need(&mut tbl_info);
    convert_old_version_utf8_to_utf8mb4_if_need(&mut tbl_info);

    for alloc in &allocs.allocs {
        alloc.transfer(db_info.id, table_id)?;
    }
    let allocs = b.build_allocs_for_create_table(tp, db_info, &tbl_info, allocs);

    let tbl_info = Arc::new(tbl_info);
    let tbl = table_from_meta(allocs, b.factory.as_ref(), tbl_info.clone())?;

    // (Go: metrics.DDLResetTempIndexWrite when all indices are public; not ported.)
    let _all_index_public = tbl_info
        .indices
        .iter()
        .flatten()
        .all(|idx| idx.state == StatePublic);

    b.add_table(schema_version, db_info, &tbl_info, tbl);

    if tbl_info.temp_table_type != TempTableNone {
        b.add_temporary_table(table_id);
    }

    // (Go appends the table to dbInfo.Deprecated.Tables when the v1 TableByID finds it:
    // never under v2.)
    Ok(affected)
}

/// ConvertCharsetCollateToLowerCaseIfNeed convert the charset / collation of table and its
/// columns to lower case, if the table's version is prior to TableInfoVersion3.
// go: ConvertCharsetCollateToLowerCaseIfNeed L1
pub fn convert_charset_collate_to_lower_case_if_need(tb_info: &mut TableInfo) {
    if tb_info.version >= TableInfoVersion3 {
        return;
    }
    tb_info.charset = to_lower(&tb_info.charset);
    tb_info.collate = to_lower(&tb_info.collate);
    for col in tb_info.columns.iter_mut().flatten() {
        let cs = to_lower(col.get_charset());
        col.set_charset(&cs);
        let co = to_lower(col.get_collate());
        col.set_collate(&co);
    }
}

/// ConvertOldVersionUTF8ToUTF8MB4IfNeed convert old version UTF8 to UTF8MB4 if
/// config.TreatOldVersionUTF8AsUTF8MB4 is enable.
// go: ConvertOldVersionUTF8ToUTF8MB4IfNeed L1
pub fn convert_old_version_utf8_to_utf8mb4_if_need(tb_info: &mut TableInfo) {
    if tb_info.version >= TableInfoVersion2
        || !tidb_base::config::get_global_config().treat_old_version_utf8_as_utf8mb4
    {
        return;
    }
    if tb_info.charset == CharsetUTF8 {
        tb_info.charset = CharsetUTF8MB4.to_owned();
        tb_info.collate = CollationUTF8MB4.to_owned();
    }
    for col in tb_info.columns.iter_mut().flatten() {
        if col.version < ColumnInfoVersion2 && col.get_charset() == CharsetUTF8 {
            col.set_charset(CharsetUTF8MB4);
            col.set_collate(CollationUTF8MB4);
        }
    }
}

impl Builder {
    /// SetSchemaVersion updates the schema version of the building infoschema.
    /// It is useful when we intentionally skip applying some schema diffs but still
    /// need to advance the schema version to reflect where the infoschema is synced to.
    // go: Builder.SetSchemaVersion L1
    pub fn set_schema_version(&mut self, version: i64) {
        Arc::make_mut(&mut self.is.misc).schema_meta_version = version;
    }

    /// Build builds and returns the built infoschema.
    // go: Builder.Build L1
    pub async fn build(mut self, schema_ts: u64) -> InfoSchemaV2 {
        self.is.ts = schema_ts;
        update_info_schema_bundles(&mut self).await;
        self.is
    }

    /// InitWithOldInfoSchema initializes an empty new InfoSchema by copies all the data
    /// from old InfoSchema. (Go first refuses to mix v1 and v2: only v2 exists here.)
    // go: Builder.InitWithOldInfoSchema L1
    pub fn init_with_old_info_schema(&mut self, old_schema: &InfoSchemaV2) -> Result<(), Error> {
        self.is.ts = old_schema.ts;
        let old_is = old_schema.base();
        self.bundle.init_bundle_info_builder();
        let misc = Arc::make_mut(&mut self.is.misc);
        misc.schema_meta_version = old_is.schema_meta_version;
        misc.rule_bundle_map = old_is.rule_bundle_map.clone();
        misc.policy_map = old_is.clone_placement_policies();
        misc.resource_group_map = old_is.clone_resource_groups();
        misc.temporary_table_ids = old_is.temporary_table_ids.clone();
        Ok(())
    }

    // go: Builder.initVirtualTables L1
    async fn init_virtual_tables(&mut self, schema_version: i64) -> Result<(), Error> {
        // Initialize virtual tables.
        let drivers: Vec<Arc<VirtualTableDriver>> = DRIVERS.read().clone();
        for driver in drivers {
            self.create_schema_tables_for_db(
                driver.db_info.clone(),
                driver.table_from_meta,
                schema_version,
            )?;
        }
        Ok(())
    }

    /// InitWithDBInfos initializes an empty new InfoSchema with a slice of DBInfo, all
    /// placement rules, and schema version.
    // go: Builder.InitWithDBInfos L1
    pub async fn init_with_db_infos(
        &mut self,
        db_infos: Vec<DBInfo>,
        policies: Vec<PolicyInfo>,
        resource_groups: Vec<ResourceGroupInfo>,
        schema_version: i64,
    ) -> Result<(), Error> {
        Arc::make_mut(&mut self.is.misc).schema_meta_version = schema_version;

        // We must not clear the historial versions like b.infoData = NewData(), because losing
        // the historial versions would cause applyDiff get db not exist error and fail, then
        // infoschema reloading retries with full load every time.
        // See https://github.com/pingcap/tidb/issues/53442
        //
        // We must reset it, otherwise the stale tables remain and cause bugs later.
        // For example, schema version 59:
        //         107: t1
        //         112: t2 (partitions p0=113, p1=114, p2=115)
        // operation: alter table t2 exchange partition p0 with table t1
        // schema version 60 if we do not reset:
        //         107: t1   <- stale
        //         112: t2 (partition p0=107, p1=114, p2=115)
        //         113: t1
        // See https://github.com/pingcap/tidb/issues/54796
        self.info_data.reset_before_full_load(schema_version);

        self.bundle.init_bundle_info_builder();

        for di in db_infos {
            self.create_schema_tables_for_db(di, table_from_meta, schema_version)?;
        }

        // initMisc depends on the tables and schemas, so it should be called after createSchemaTablesForDB
        self.init_misc(policies, resource_groups);

        self.init_virtual_tables(schema_version).await?;

        // (Go: sortAllTablesByID, the v1 buckets.)
        Ok(())
    }
}

/// The CachedTable branch of Go (`t.Init(factory session's SQLExecutor)`, loading the
/// cache through an internal session) is D06.4 part 2 (`cache.go`): a cached table is the
/// part-1 [`table::CachedTable`] marker without a cache, so it is returned without `Init`
/// and its readers stop at their boundary stubs (blocked-by:D06.4).
// go: tableFromMeta L1
pub(crate) fn table_from_meta(
    alloc: Allocators,
    _factory: Option<&Factory>,
    tbl_info: Arc<TableInfo>,
) -> Result<TableRef, Error> {
    let ret = table::table_from_meta(alloc, tbl_info)?;
    Ok(ret)
}

/// The function that builds a table of a schema (Go `tableFromMetaFunc`).
pub type TableFromMetaFunc =
    fn(Allocators, Option<&Factory>, Arc<TableInfo>) -> Result<TableRef, Error>;

impl Builder {
    // go: Builder.createSchemaTablesForDB L1 [M-MAPORDER]
    fn create_schema_tables_for_db(
        &mut self,
        mut di: DBInfo,
        table_from_meta: TableFromMetaFunc,
        schema_version: i64,
    ) -> Result<(), Error> {
        let mut tables: HashMap<String, TableRef> =
            HashMap::with_capacity(di.deprecated.tables.as_ref().map_or(0, Vec::len));
        let special = is_special_db(&di.name.l);
        let deprecated_tables: Vec<Arc<TableInfo>> = di
            .deprecated
            .tables
            .take()
            .into_iter()
            .flatten()
            .map(Arc::new)
            .collect();
        for t in &deprecated_tables {
            let allocs = autoid::new_allocators_from_tbl_info(self.requirement.as_ref(), di.id, t);
            let tbl = table_from_meta(allocs, self.factory.as_ref(), t.clone()).map_err(|err| {
                tidb_base::errors::errorf(
                    "%s: %s",
                    &[
                        Arg::from(format!(
                            "Build table `{}`.`{}` schema failed",
                            di.name.o, t.name.o
                        )),
                        Arg::Error(err.error()),
                    ],
                )
            })?;

            tables.insert(t.name.l.clone(), tbl.clone());
            self.add_table(schema_version, &di, t, tbl.clone());
            if let Some(m) = &mut di.table_name2_id
                && !m.is_empty()
            {
                m.remove(&t.name.l);
            }

            let tbl_info = tbl.meta();
            if tbl_info.temp_table_type != TempTableNone {
                self.add_temporary_table(tbl_info.id);
            }
        }
        // Add the rest name to ID mappings.
        for (name, &id) in di.table_name2_id.iter().flatten() {
            let item = TableItem {
                db_name: di.name.clone(),
                db_id: di.id,
                table_name: CIStr::new(name),
                table_id: id,
                schema_version,
                tomb: false,
            };
            self.info_data.add_name_only(item);
        }
        if special {
            // Go's addSpecialDB keeps the DBInfo with its tables (the same pointers).
            di.deprecated.tables = Some(deprecated_tables.iter().map(|t| t.go_copy()).collect());
        }
        self.add_db(schema_version, di, tables);

        Ok(())
    }

    // go: Builder.addDB L1
    pub(crate) fn add_db(
        &mut self,
        schema_version: i64,
        di: DBInfo,
        tables: HashMap<String, TableRef>,
    ) {
        if is_special_db(&di.name.l) {
            let di = Arc::new(di);
            self.info_data.add_special_db(
                &di.clone(),
                Arc::new(SchemaTables {
                    db_info: di,
                    tables,
                }),
            );
        } else {
            self.info_data.add_db(schema_version, di);
        }
    }

    // go: Builder.addTable L1
    pub(crate) fn add_table(
        &mut self,
        schema_version: i64,
        di: &DBInfo,
        tbl_info: &TableInfo,
        tbl: TableRef,
    ) {
        self.info_data
            .add_referred_foreign_keys(&di.name, tbl_info, schema_version);
        self.info_data.add(
            TableItem {
                db_name: di.name.clone(),
                db_id: di.id,
                table_name: tbl_info.name.clone(),
                table_id: tbl_info.id,
                schema_version,
                tomb: false,
            },
            tbl,
        );
    }
}

/// A memory schema and the function that builds its tables.
pub struct VirtualTableDriver {
    pub db_info: DBInfo,
    pub table_from_meta: TableFromMetaFunc,
}

/// The memory schemas (`drivers`): registered by D06.6 (information_schema,
/// performance_schema, metrics_schema).
static DRIVERS: LazyLock<RwLock<Vec<Arc<VirtualTableDriver>>>> =
    LazyLock::new(|| RwLock::new(Vec::new()));

/// RegisterVirtualTable register virtual tables to the builder.
// go: RegisterVirtualTable L1
pub fn register_virtual_table(db_info: DBInfo, table_from_meta: TableFromMetaFunc) {
    DRIVERS.write().push(Arc::new(VirtualTableDriver {
        db_info,
        table_from_meta,
    }));
}

/// NewBuilder creates a new Builder with a Handle. Go's `useV2` is always true here
/// (spec §1.2); the cache capacity is `tidb_schema_cache_size` (0: unbounded, A3 §5.4).
// go: NewBuilder L2 (always v2; capacity 0 is unbounded, A3)
pub fn new_builder(
    r: Option<Arc<dyn Requirement>>,
    factory: Option<Factory>,
    info_data: Arc<Data>,
) -> Builder {
    let builder = Builder {
        requirement: r.clone(),
        is: new_info_schema_v2(r, factory.clone(), info_data.clone()),
        factory,
        bundle: BundleInfoBuilder::default(),
        info_data: info_data.clone(),
        store: None,
        refuse_unknown_actions: false,
    };
    let schema_cache_size = tidb_sessionctx::variable::tidb_vars::SchemaCacheSize
        .load(std::sync::atomic::Ordering::SeqCst);
    info_data
        .table_cache
        .set_capacity(schema_cache_capacity(schema_cache_size));
    builder
}

impl Builder {
    /// SetDeltaUpdateBundles (promoted from the embedded bundleInfoBuilder).
    // glue: Go promoted method of the embedded bundleInfoBuilder
    pub fn set_delta_update_bundles(&mut self) {
        self.bundle.set_delta_update_bundles();
    }

    /// WithStore attaches the given store to builder.
    // go: Builder.WithStore L1
    pub fn with_store(mut self, s: Arc<dyn Storage>) -> Self {
        self.store = Some(s);
        self
    }

    /// schemaByID returns schema by ID, respecting the enableV2 flag
    // go: Builder.schemaByID L1
    pub(crate) fn schema_by_id(&self, id: i64) -> Option<Arc<DBInfo>> {
        self.is.schema_by_id(id)
    }
}

// go: tableIDIsValid L1
pub(crate) fn table_id_is_valid(table_id: i64) -> bool {
    table_id > 0
}

#[cfg(test)]
mod tests {
    //! go-file: pkg/infoschema/builder_test.go
    use super::*;
    use tidb_model::ActionAddColumn;

    struct MockAlloc(AllocatorType);

    impl Allocator for MockAlloc {
        fn get_type(&self) -> AllocatorType {
            self.0
        }
        fn transfer(&self, _: i64, _: i64) -> Result<(), Error> {
            Ok(())
        }
    }

    #[test]
    fn test_get_kept_allocators() {
        let allocators = Allocators {
            sep_auto_inc: false,
            allocs: vec![
                Arc::new(MockAlloc(RowIDAllocType)),
                Arc::new(MockAlloc(AutoIncrementType)),
                Arc::new(MockAlloc(AutoRandomType)),
            ],
        };
        let diff = |tp: ActionType, sub: &[ActionType]| SchemaDiff {
            type_: tp,
            sub_action_types: (!sub.is_empty()).then(|| sub.iter().map(|t| t.0).collect()),
            ..Default::default()
        };
        let all = vec![RowIDAllocType, AutoIncrementType, AutoRandomType];
        let cases = [
            (diff(ActionTruncateTable, &[]), all.clone()),
            (diff(ActionRebaseAutoID, &[]), vec![AutoRandomType]),
            (
                diff(ActionModifyTableAutoIDCache, &[]),
                vec![AutoRandomType],
            ),
            (
                diff(ActionRebaseAutoRandomBase, &[]),
                vec![RowIDAllocType, AutoIncrementType],
            ),
            (
                diff(
                    ActionMultiSchemaChange,
                    &[ActionAddColumn, ActionRebaseAutoID],
                ),
                vec![AutoRandomType],
            ),
            (
                diff(ActionMultiSchemaChange, &[ActionModifyTableAutoIDCache]),
                vec![AutoRandomType],
            ),
            (
                diff(ActionMultiSchemaChange, &[ActionRebaseAutoRandomBase]),
                vec![RowIDAllocType, AutoIncrementType],
            ),
            (
                diff(ActionMultiSchemaChange, &[ActionAddColumn]),
                all.clone(),
            ),
        ];
        for (i, (d, expected)) in cases.iter().enumerate() {
            let res = get_kept_allocators(d, allocators.clone());
            let got: Vec<AllocatorType> = res.allocs.iter().map(|a| a.get_type()).collect();
            assert_eq!(&got, expected, "case {i}");
        }
    }
}
