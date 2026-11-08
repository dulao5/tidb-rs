//! go-file: pkg/meta/model/job.go
//!
//! The functions of `job.go` that the read path keeps (block D06.1 part 2; the reachability
//! analysis of tracker PR #10 put the DDL write path, 219 functions, in X: DDL jobs are run
//! by Go TiDB). The structs and their JSON form are generated (`types_gen.rs`).
//!
//! Mapping:
//! - `Job.Mu` (a `sync.Mutex` guarding `RowCount` and the reorg warnings against the DDL
//!   worker) has no Rust counterpart: tidb-rs reads jobs it decoded itself, and `&self`
//!   borrows exclude a concurrent writer (L2).
//! - `Job.args` (the decoded arguments, filled only by `FillArgs` / `decodeArgs`, X) is not
//!   held: `ArgLen` in `Job.String` is 0, as for every job Go decodes and does not fill.

#![allow(non_upper_case_globals)]

use std::collections::BTreeMap;

use tidb_base::Error;

use crate::bdr::ts_convert2_time;
use crate::column::sl;
use crate::{
    _DEPRECATEDActionAlterTableAlterPartition, ActionAddCheckConstraint, ActionAddColumn,
    ActionAddForeignKey, ActionAddIndex, ActionAddPrimaryKey, ActionAddTablePartition,
    ActionAddVectorIndex, ActionAlterCacheTable, ActionAlterCheckConstraint,
    ActionAlterIndexVisibility, ActionAlterNoCacheTable, ActionAlterPlacementPolicy,
    ActionAlterResourceGroup, ActionAlterSequence, ActionAlterTTLInfo, ActionAlterTTLRemove,
    ActionAlterTableAffinity, ActionAlterTableAttributes, ActionAlterTableMode,
    ActionAlterTablePartitionAttributes, ActionAlterTablePartitionPlacement,
    ActionAlterTablePartitioning, ActionAlterTablePlacement, ActionAlterTableSoftDeleteInfo,
    ActionAlterTableStatsOptions, ActionCreatePlacementPolicy, ActionCreateResourceGroup,
    ActionCreateSchema, ActionCreateSequence, ActionCreateTable, ActionCreateTables,
    ActionCreateView, ActionDropCheckConstraint, ActionDropColumn, ActionDropForeignKey,
    ActionDropIndex, ActionDropPlacementPolicy, ActionDropPrimaryKey, ActionDropResourceGroup,
    ActionDropSchema, ActionDropSequence, ActionDropTable, ActionDropTablePartition,
    ActionDropView, ActionExchangeTablePartition, ActionFlashbackCluster, ActionLockTable,
    ActionModifyColumn, ActionModifySchemaCharsetAndCollate, ActionModifySchemaDefaultPlacement,
    ActionModifySchemaSoftDeleteAndActiveActive, ActionModifyTableAutoIDCache,
    ActionModifyTableCharsetAndCollate, ActionModifyTableComment, ActionMultiSchemaChange,
    ActionRebaseAutoID, ActionRebaseAutoRandomBase, ActionRecoverSchema, ActionRecoverTable,
    ActionRefreshMeta, ActionRemovePartitioning, ActionRenameIndex, ActionRenameTable,
    ActionRenameTables, ActionReorganizePartition, ActionRepairTable, ActionSetDefaultValue,
    ActionSetTiFlashReplica, ActionShardRowID, ActionTruncateTable, ActionTruncateTablePartition,
    ActionType, ActionUnlockTable, ActionUpdateTiFlashReplicaStatus, Job, JobState,
    JobStateCancelled, JobStateCancelling, JobStateDone, JobStatePaused, JobStatePausing,
    JobStateQueueing, JobStateRollbackDone, JobStateRollingback, JobStateRunning, JobStateSynced,
    JobVersion, JobVersion1, JobVersion2, SchemaState, StateDeleteOnly, StateDeleteReorganization,
    StateGlobalTxnOnly, StatePublic, StateReplicaOnly, StateWriteOnly, StateWriteReorganization,
};

/// ActionMap is the map of DDL ActionType to string (a Go map literal; looked up by key).
pub static ActionMap: &[(ActionType, &str)] = &[
    (ActionCreateSchema, "create schema"),
    (ActionDropSchema, "drop schema"),
    (ActionCreateTable, "create table"),
    (ActionCreateTables, "create tables"),
    (ActionDropTable, "drop table"),
    (ActionAddColumn, "add column"),
    (ActionDropColumn, "drop column"),
    (ActionAddIndex, "add index"),
    (ActionDropIndex, "drop index"),
    (ActionAddForeignKey, "add foreign key"),
    (ActionDropForeignKey, "drop foreign key"),
    (ActionTruncateTable, "truncate table"),
    (ActionModifyColumn, "modify column"),
    (ActionRebaseAutoID, "rebase auto_increment ID"),
    (ActionRenameTable, "rename table"),
    (ActionRenameTables, "rename tables"),
    (ActionSetDefaultValue, "set default value"),
    (ActionShardRowID, "shard row ID"),
    (ActionModifyTableComment, "modify table comment"),
    (ActionRenameIndex, "rename index"),
    (ActionAddTablePartition, "add partition"),
    (ActionDropTablePartition, "drop partition"),
    (ActionCreateView, "create view"),
    (
        ActionModifyTableCharsetAndCollate,
        "modify table charset and collate",
    ),
    (ActionTruncateTablePartition, "truncate partition"),
    (ActionDropView, "drop view"),
    (ActionRecoverTable, "recover table"),
    (
        ActionModifySchemaCharsetAndCollate,
        "modify schema charset and collate",
    ),
    (ActionLockTable, "lock table"),
    (ActionUnlockTable, "unlock table"),
    (ActionRepairTable, "repair table"),
    (ActionSetTiFlashReplica, "set tiflash replica"),
    (
        ActionUpdateTiFlashReplicaStatus,
        "update tiflash replica status",
    ),
    (ActionAddPrimaryKey, "add primary key"),
    (ActionDropPrimaryKey, "drop primary key"),
    (ActionCreateSequence, "create sequence"),
    (ActionAlterSequence, "alter sequence"),
    (ActionDropSequence, "drop sequence"),
    (ActionModifyTableAutoIDCache, "modify auto id cache"),
    (ActionRebaseAutoRandomBase, "rebase auto_random ID"),
    (ActionAlterIndexVisibility, "alter index visibility"),
    (ActionExchangeTablePartition, "exchange partition"),
    (ActionAddCheckConstraint, "add check constraint"),
    (ActionDropCheckConstraint, "drop check constraint"),
    (ActionAlterCheckConstraint, "alter check constraint"),
    (ActionAlterTableAttributes, "alter table attributes"),
    (
        ActionAlterTablePartitionPlacement,
        "alter table partition placement",
    ),
    (
        ActionAlterTablePartitionAttributes,
        "alter table partition attributes",
    ),
    (ActionCreatePlacementPolicy, "create placement policy"),
    (ActionAlterPlacementPolicy, "alter placement policy"),
    (ActionDropPlacementPolicy, "drop placement policy"),
    (
        ActionModifySchemaDefaultPlacement,
        "modify schema default placement",
    ),
    (ActionAlterTablePlacement, "alter table placement"),
    (ActionAlterCacheTable, "alter table cache"),
    (ActionAlterNoCacheTable, "alter table nocache"),
    (
        ActionAlterTableStatsOptions,
        "alter table statistics options",
    ),
    (ActionMultiSchemaChange, "alter table multi-schema change"),
    (ActionFlashbackCluster, "flashback cluster"),
    (ActionRecoverSchema, "flashback schema"),
    (
        ActionReorganizePartition,
        "alter table reorganize partition",
    ),
    (ActionAlterTTLInfo, "alter table ttl"),
    (ActionAlterTTLRemove, "alter table no_ttl"),
    (ActionCreateResourceGroup, "create resource group"),
    (ActionAlterResourceGroup, "alter resource group"),
    (ActionDropResourceGroup, "drop resource group"),
    (ActionAlterTablePartitioning, "alter table partition by"),
    (ActionRemovePartitioning, "alter table remove partitioning"),
    (ActionAddVectorIndex, "add vector index"),
    (ActionAlterTableMode, "alter table mode"),
    (ActionRefreshMeta, "refresh meta"),
    (ActionAlterTableAffinity, "alter table affinity"),
    (ActionAlterTableSoftDeleteInfo, "alter soft delete info"),
    (
        ActionModifySchemaSoftDeleteAndActiveActive,
        "modify schema soft delete and active active",
    ),
    (_DEPRECATEDActionAlterTableAlterPartition, "alter partition"),
];

impl ActionType {
    /// String return current ddl action in string
    // go: ActionType.String L1 [M-NAME]
    pub fn to_string(self) -> &'static str {
        ActionMap
            .iter()
            .find(|(a, _)| *a == self)
            .map_or("none", |(_, v)| v)
    }
}

impl SchemaState {
    /// String implements fmt.Stringer interface.
    // go: SchemaState.String L1 [M-NAME]
    pub fn to_string(self) -> &'static str {
        match self {
            StateDeleteOnly => "delete only",
            StateWriteOnly => "write only",
            StateWriteReorganization => "write reorganization",
            StateDeleteReorganization => "delete reorganization",
            StatePublic => "public",
            StateReplicaOnly => "replica only",
            StateGlobalTxnOnly => "global txn only",
            _ => "none",
        }
    }
}

impl JobVersion {
    /// String implements fmt.Stringer interface.
    // go: JobVersion.String L1 [M-NAME]
    #[allow(clippy::inherent_to_string)]
    pub fn to_string(self) -> String {
        if self == JobVersion1 {
            return "v1".to_owned();
        } else if self == JobVersion2 {
            return "v2".to_owned();
        }
        format!("unknown({})", self.0)
    }
}

impl JobState {
    /// String implements fmt.Stringer interface.
    // go: JobState.String L1 [M-NAME]
    pub fn to_string(self) -> &'static str {
        match self {
            JobStateRunning => "running",
            JobStateRollingback => "rollingback",
            JobStateRollbackDone => "rollback done",
            JobStateDone => "done",
            JobStateCancelled => "cancelled",
            JobStateCancelling => "cancelling",
            JobStateSynced => "synced",
            JobStateQueueing => "queueing",
            JobStatePaused => "paused",
            JobStatePausing => "pausing",
            _ => "none",
        }
    }
}

/// The reorg warnings of a job and their counts (`map[errors.ErrorID]*terror.Error`,
/// `map[errors.ErrorID]int64`; `None` is a nil map).
pub type JobWarnings<'a> = (
    &'a Option<BTreeMap<String, Option<Error>>>,
    &'a Option<BTreeMap<String, i64>>,
);

impl Job {
    /// GetRowCount gets the number of rows.
    // go: Job.GetRowCount L2 (no Job.Mu: &self excludes the writer)
    pub fn get_row_count(&self) -> i64 {
        self.row_count
    }

    /// GetWarnings gets the warnings of the rows handled. Panics when the job has no reorg
    /// meta (Go dereferences the nil `ReorgMeta`).
    // go: Job.GetWarnings L2 (no Job.Mu: &self excludes the writer; nil ReorgMeta panics [M-PANIC])
    pub fn get_warnings(&self) -> JobWarnings<'_> {
        let rm = self
            .reorg_meta
            .as_deref()
            .expect("runtime error: invalid memory address or nil pointer dereference");
        (&rm.warnings, &rm.warnings_count)
    }

    /// Decode decodes job from the json buffer, we must use decodeArgs later to
    /// decode special args for this job.
    // go: Job.Decode L1
    pub fn decode(&mut self, b: &[u8]) -> Result<(), Error> {
        self.unmarshal_json(b)
    }

    /// String implements fmt.Stringer interface.
    // go: Job.String L2 (ArgLen is 0: Job.args is not held, see the module doc [M-NAME])
    #[allow(clippy::inherent_to_string)]
    pub fn to_string(&self) -> String {
        let row_count = self.get_row_count();
        let err = match &self.error {
            None => "<nil>".to_owned(),
            Some(e) => String::from_utf8_lossy(&e.error()).into_owned(),
        };
        let mut ret = format!(
            "ID:{}, Type:{}, State:{}, SchemaState:{}, SchemaID:{}, TableID:{}, RowCount:{}, ArgLen:{}, start time: {}, Err:{}, ErrCount:{}, SnapshotVersion:{}, Version: {}",
            self.id,
            self.type_.to_string(),
            self.state.to_string(),
            self.schema_state.to_string(),
            self.schema_id,
            self.table_id,
            row_count,
            0,
            ts_convert2_time(self.start_ts).string(),
            err,
            self.error_count,
            self.snapshot_ver,
            self.version.to_string(),
        );
        if let Some(rm) = &self.reorg_meta {
            if self.type_ == ActionModifyColumn {
                ret += &format!(", analyze_state:{}", rm.analyze_state);
                ret += &format!(", stage:{}", rm.stage.0);
            }
            let (warnings, _) = self.get_warnings();
            ret += &format!(
                ", UniqueWarnings:{}",
                warnings.as_ref().map_or(0, BTreeMap::len)
            );
        }
        if self.type_ != ActionMultiSchemaChange
            && let Some(msi) = &self.multi_schema_info
        {
            ret += &format!(", Multi-Schema Change:true, Revertible:{}", msi.revertible);
        }
        ret
    }

    /// MayNeedReorg indicates that this job may need to reorganize the data. Panics for a
    /// multi-schema change without `MultiSchemaInfo` (Go dereferences nil).
    // go: Job.MayNeedReorg L1 [M-PANIC]
    pub fn may_need_reorg(&self) -> bool {
        match self.type_ {
            ActionAddIndex
            | ActionAddPrimaryKey
            | ActionReorganizePartition
            | ActionRemovePartitioning
            | ActionAlterTablePartitioning => true,
            ActionModifyColumn => self.need_reorg,
            ActionMultiSchemaChange => {
                let msi = self
                    .multi_schema_info
                    .as_deref()
                    .expect("runtime error: invalid memory address or nil pointer dereference");
                for sub in sl(&msi.sub_jobs) {
                    let proxy_job = Job {
                        type_: sub.type_,
                        need_reorg: sub.need_reorg,
                        ..Job::default()
                    };
                    if proxy_job.may_need_reorg() {
                        return true;
                    }
                }
                false
            }
            _ => false,
        }
    }
}
