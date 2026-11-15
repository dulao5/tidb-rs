//! go-file: pkg/meta/model/table.go

#![allow(non_upper_case_globals)]

use std::collections::BTreeMap;
use std::sync::{Arc, LazyLock};

use tidb_base::gostd::time::{Duration, Time};
use tidb_base::{Error, errors, gofmt::Arg, mysql};
use tidb_parser::model::{
    CIStr, PartitionTypeList, PartitionTypeNone, PartitionTypeRange, ReferOptionNoOption,
    ReferOptionType,
};

use crate::column::{find_column_info_by_id, sl};
use crate::gocopy::make_mut;
use crate::index::find_index_info_by_id;
use crate::{
    ActionAddTablePartition, ActionDropTablePartition, ActionNone, ActionTruncateTablePartition,
    ColumnInfo, ConstraintInfo, Day, DefaultJobInterval, FKInfo, IndexInfo, Month, Never,
    PartitionDefinition, PartitionInfo, PartitionState, SchemaState, SessionInfo, SoftdeleteInfo,
    StateDeleteOnly, StateDeleteReorganization, StateNone, StatePublic, StateWriteOnly,
    StatsLoadItem, StatsOptions, TTLInfo, TableAffinityInfo, TableCacheStatusDisable,
    TableCacheStatusEnable, TableCacheStatusSwitching, TableCacheStatusType, TableInfo,
    TableInfoVersion5, TableItemID, TableLockState, TableLockStatePreLock, TableLockStatePublic,
    TableMode, TableModeImport, TableModeNormal, TableModeRestore, TempTableGlobal, TempTableLocal,
    TempTableType, TiFlashReplicaInfo, Week, WindowRepeatType, emptyPartitionDefinitionSize,
};

/// ExtraHandleName is the name of ExtraHandle Column.
pub static ExtraHandleName: LazyLock<CIStr> = LazyLock::new(|| CIStr::new("_tidb_rowid"));

/// ExtraPhysTblIDName is the name of ExtraPhysTblID Column.
pub static ExtraPhysTblIDName: LazyLock<CIStr> = LazyLock::new(|| CIStr::new("_tidb_tid"));

impl TableInfo {
    /// SepAutoInc decides whether _rowid and auto_increment id use separate allocator.
    // go: TableInfo.SepAutoInc L1
    pub fn sep_auto_inc(&self) -> bool {
        self.version >= TableInfoVersion5 && self.auto_id_cache == 1
    }

    /// GetPartitionInfo returns the partition information.
    // go: TableInfo.GetPartitionInfo L1
    pub fn get_partition_info(&self) -> Option<&PartitionInfo> {
        match &self.partition {
            Some(p) if p.enable => Some(p),
            _ => None,
        }
    }

    /// GetUpdateTime gets the table's updating time.
    // go: TableInfo.GetUpdateTime L1
    pub fn get_update_time(&self) -> Time {
        crate::bdr::ts_convert2_time(self.update_ts)
    }

    /// GetPkName will return the pk name if pk exists.
    // go: TableInfo.GetPkName L1
    pub fn get_pk_name(&self) -> CIStr {
        for col_info in sl(&self.columns) {
            if mysql::has_pri_key_flag(col_info.get_flag()) {
                return col_info.name.clone();
            }
        }
        CIStr::default()
    }

    /// GetPkColInfo gets the ColumnInfo of pk if exists. Make sure PkIsHandle checked before
    /// call this method.
    // go: TableInfo.GetPkColInfo L1
    pub fn get_pk_col_info(&self) -> Option<&Arc<ColumnInfo>> {
        sl(&self.columns)
            .iter()
            .find(|c| mysql::has_pri_key_flag(c.get_flag()))
    }

    /// GetAutoIncrementColInfo gets the ColumnInfo of auto_increment column if exists.
    // go: TableInfo.GetAutoIncrementColInfo L1
    pub fn get_auto_increment_col_info(&self) -> Option<&Arc<ColumnInfo>> {
        sl(&self.columns)
            .iter()
            .find(|c| mysql::has_auto_increment_flag(c.get_flag()))
    }

    /// IsAutoIncColUnsigned checks whether the auto increment column is unsigned.
    // go: TableInfo.IsAutoIncColUnsigned L1
    pub fn is_auto_inc_col_unsigned(&self) -> bool {
        match self.get_auto_increment_col_info() {
            None => false,
            Some(col) => mysql::has_unsigned_flag(col.get_flag()),
        }
    }

    /// ContainsAutoRandomBits indicates whether a table contains auto_random column.
    // go: TableInfo.ContainsAutoRandomBits L1
    pub fn contains_auto_random_bits(&self) -> bool {
        self.auto_random_bits != 0
    }

    /// IsAutoRandomBitColUnsigned indicates whether the auto_random column is unsigned. Make
    /// sure the table contains auto_random before calling this method. Panics when there is
    /// no primary key column, as Go does.
    // go: TableInfo.IsAutoRandomBitColUnsigned L1 [M-PANIC]
    pub fn is_auto_random_bit_col_unsigned(&self) -> bool {
        if !self.pk_is_handle || self.auto_random_bits == 0 {
            return false;
        }
        mysql::has_unsigned_flag(
            self.get_pk_col_info()
                .expect("invalid memory address or nil pointer dereference")
                .get_flag(),
        )
    }

    /// Cols returns the columns of the table in public state: a slice indexed by offset, up
    /// to the largest public offset. Go leaves a nil at an offset whose column is not public;
    /// that is `None` here. Panics on an offset out of range, as Go does.
    // go: TableInfo.Cols L1
    pub fn cols(&self) -> Vec<Option<&Arc<ColumnInfo>>> {
        let columns = sl(&self.columns);
        let mut public_columns: Vec<Option<&Arc<ColumnInfo>>> = vec![None; columns.len()];
        let mut max_offset: isize = -1;
        for col in columns {
            if col.state != StatePublic {
                continue;
            }
            public_columns[col.offset as usize] = Some(col);
            if max_offset < col.offset {
                max_offset = col.offset;
            }
        }
        public_columns.truncate((max_offset + 1) as usize);
        public_columns
    }

    /// FindIndexByName finds index by name.
    // go: TableInfo.FindIndexByName L1
    pub fn find_index_by_name(&self, idx_name: &str) -> Option<&Arc<IndexInfo>> {
        sl(&self.indices).iter().find(|i| i.name.l == idx_name)
    }

    /// FindColumnByID finds ColumnInfo by id.
    // go: TableInfo.FindColumnByID L1
    pub fn find_column_by_id(&self, id: i64) -> Option<&Arc<ColumnInfo>> {
        sl(&self.columns).iter().find(|c| c.id == id)
    }

    /// FindIndexByID finds index by id.
    // go: TableInfo.FindIndexByID L1
    pub fn find_index_by_id(&self, id: i64) -> Option<&Arc<IndexInfo>> {
        sl(&self.indices).iter().find(|i| i.id == id)
    }

    /// FindPublicColumnByName finds the public column by name. A non-public hole in
    /// `Cols()` panics, as the nil dereference does in Go.
    // go: TableInfo.FindPublicColumnByName L1 [M-PANIC]
    pub fn find_public_column_by_name(&self, col_name_l: &str) -> Option<&Arc<ColumnInfo>> {
        for col in self.cols() {
            let col = col.expect("invalid memory address or nil pointer dereference");
            if col.name.l == col_name_l {
                return Some(col);
            }
        }
        None
    }

    /// IsLocked checks whether the table was locked.
    // go: TableInfo.IsLocked L1
    pub fn is_locked(&self) -> bool {
        self.lock
            .as_ref()
            .is_some_and(|l| !sl(&l.sessions).is_empty())
    }

    /// MoveColumnInfo moves a column to another offset. It maintains the offsets of all
    /// affects columns and index columns.
    // go: TableInfo.MoveColumnInfo L1 [M-SHARE]
    pub fn move_column_info(&mut self, from: usize, to: usize) {
        if from == to {
            return;
        }
        // Update column offsets.
        let mut updated_offsets: BTreeMap<isize, isize> = BTreeMap::new();
        let columns = self.columns.get_or_insert_with(Vec::new);
        let src = Arc::clone(&columns[from]);
        if from < to {
            for i in from..to {
                columns[i] = Arc::clone(&columns[i + 1]);
                make_mut(&mut columns[i]).offset = i as isize;
                updated_offsets.insert((i + 1) as isize, i as isize);
            }
        } else if from > to {
            for i in (to + 1..=from).rev() {
                columns[i] = Arc::clone(&columns[i - 1]);
                make_mut(&mut columns[i]).offset = i as isize;
                updated_offsets.insert((i - 1) as isize, i as isize);
            }
        }
        columns[to] = src;
        make_mut(&mut columns[to]).offset = to as isize;

        // Update index column offsets.
        updated_offsets.insert(from as isize, to as isize);
        for idx in self.indices.iter_mut().flatten() {
            // Copy on write only an index whose offsets change (N13 §3 rule 2): the others
            // stay shared with the other holders of this `TableInfo`'s indices.
            let moved = |o: &isize| updated_offsets.contains_key(o);
            if !idx.columns.iter().flatten().any(|c| moved(&c.offset))
                && !idx.affect_column.iter().flatten().any(|c| moved(&c.offset))
            {
                continue;
            }
            let idx = make_mut(idx);
            for idx_col in idx.columns.iter_mut().flatten() {
                if let Some(&new_offset) = updated_offsets.get(&idx_col.offset) {
                    idx_col.offset = new_offset;
                }
            }
            for affected_col in idx.affect_column.iter_mut().flatten() {
                if let Some(&new_offset) = updated_offsets.get(&affected_col.offset) {
                    affected_col.offset = new_offset;
                }
            }
        }

        // Reconstruct the dependency column offsets.
        for col in self.columns.iter_mut().flatten() {
            if let Some(&new_offset) = col
                .change_state_info
                .as_ref()
                .and_then(|csi| updated_offsets.get(&csi.dependency_column_offset))
                && let Some(csi) = &mut make_mut(col).change_state_info
            {
                csi.dependency_column_offset = new_offset;
            }
        }
    }

    /// ClearPlacement clears all table and partitions' placement settings.
    // go: TableInfo.ClearPlacement L1
    pub fn clear_placement(&mut self) {
        self.placement_policy_ref = None;
        if let Some(p) = &mut self.partition {
            for def in p.definitions.iter_mut().flatten() {
                def.placement_policy_ref = None;
            }
        }
    }

    /// GetPrimaryKey extract the primary key in a table and return `IndexInfo`. The returned
    /// primary key could be explicit or implicit. If there is no explicit primary key in
    /// table, the first UNIQUE INDEX on NOT NULL column(s) will be taken.
    // go: TableInfo.GetPrimaryKey L1
    pub fn get_primary_key(&self) -> Option<&Arc<IndexInfo>> {
        let mut implicit_pk: Option<&Arc<IndexInfo>> = None;
        for key in sl(&self.indices) {
            if key.primary {
                // table has explicit primary key
                return Some(key);
            }
            // The case index without any columns should never happen, but still do a check
            // here
            if sl(&key.columns).is_empty() {
                continue;
            }
            // find the first unique key with NOT NULL columns
            if implicit_pk.is_none() && key.unique {
                // ensure all columns in unique key have NOT NULL flag
                let mut all_col_not_null = true;
                let mut skip = false;
                for idx_col in sl(&key.columns) {
                    let public = self.cols();
                    let col = find_public_hole_tolerant(&public, &idx_col.name.l);
                    // This index has a column in DeleteOnly state, or it is expression index
                    // (it defined on a hidden column), it can not be implicit PK, go to next
                    // index iterator
                    match col {
                        None => {
                            skip = true;
                            break;
                        }
                        Some(c) if c.hidden => {
                            skip = true;
                            break;
                        }
                        Some(c) => {
                            if !mysql::has_not_null_flag(c.get_flag()) {
                                all_col_not_null = false;
                                break;
                            }
                        }
                    }
                }
                if skip {
                    continue;
                }
                if all_col_not_null {
                    implicit_pk = Some(key);
                }
            }
        }
        implicit_pk
    }

    /// ColumnIsInIndex checks whether c is included in any indices of t.
    // go: TableInfo.ColumnIsInIndex L1
    pub fn column_is_in_index(&self, c: &ColumnInfo) -> bool {
        sl(&self.indices).iter().any(|index| {
            sl(&index.columns)
                .iter()
                .any(|column| column.name.l == c.name.l)
        })
    }

    /// HasClusteredIndex checks whether the table has a clustered index.
    // go: TableInfo.HasClusteredIndex L1
    pub fn has_clustered_index(&self) -> bool {
        self.pk_is_handle || self.is_common_handle
    }

    /// IsView checks if TableInfo is a view.
    // go: TableInfo.IsView L1
    pub fn is_view(&self) -> bool {
        self.view.is_some()
    }

    /// IsSequence checks if TableInfo is a sequence.
    // go: TableInfo.IsSequence L1
    pub fn is_sequence(&self) -> bool {
        self.sequence.is_some()
    }

    /// IsBaseTable checks to see the table is neither a view nor a sequence.
    // go: TableInfo.IsBaseTable L1
    pub fn is_base_table(&self) -> bool {
        self.sequence.is_none() && self.view.is_none()
    }

    /// FindConstraintInfoByName finds constraintInfo by name.
    // go: TableInfo.FindConstraintInfoByName L1
    pub fn find_constraint_info_by_name(&self, constr_name: &str) -> Option<&ConstraintInfo> {
        let low_constr_name = tidb_base::gostd::to_lower(constr_name);
        sl(&self.constraints)
            .iter()
            .find(|c| c.name.l == low_constr_name)
    }

    /// FindIndexNameByID finds index name by id.
    // go: TableInfo.FindIndexNameByID L1
    pub fn find_index_name_by_id(&self, id: i64) -> String {
        find_index_info_by_id(sl(&self.indices), id).map_or_else(String::new, |i| i.name.l.clone())
    }

    /// FindColumnNameByID finds column name by id.
    // go: TableInfo.FindColumnNameByID L1
    pub fn find_column_name_by_id(&self, id: i64) -> String {
        find_column_info_by_id(sl(&self.columns), id).map_or_else(String::new, |c| c.name.l.clone())
    }

    /// GetColumnByID finds the public ColumnInfo by id.
    // go: TableInfo.GetColumnByID L1
    pub fn get_column_by_id(&self, id: i64) -> Option<&Arc<ColumnInfo>> {
        sl(&self.columns)
            .iter()
            .find(|c| c.state == StatePublic && c.id == id)
    }

    /// GetNonTempColumns returns the columns that are neither being removed nor being
    /// changed (the originals of changing columns are dropped). Go returns them in map
    /// iteration order (random); here they are in name order (M-MAPORDER).
    // go: TableInfo.GetNonTempColumns L1 [M-MAPORDER]
    pub fn get_non_temp_columns(&self) -> Vec<&Arc<ColumnInfo>> {
        let mut col_map: BTreeMap<&str, &Arc<ColumnInfo>> = BTreeMap::new();
        for col in sl(&self.columns) {
            if col.is_removing() {
                continue;
            }
            col_map.insert(&col.name.l, col);
        }
        for col in sl(&self.columns) {
            if col.is_removing() {
                continue;
            }
            if col.is_changing() {
                col_map.remove(col.get_changing_origin_name());
            }
        }
        col_map.into_values().collect()
    }
}

/// `FindColumnInfo(t.Cols(), name)` where `Cols()` may hold nil entries: Go dereferences
/// `col.Name.L` on a nil entry and panics; so does this.
// go-part: TableInfo.GetPrimaryKey
fn find_public_hole_tolerant<'a>(
    cols: &[Option<&'a Arc<ColumnInfo>>],
    name: &str,
) -> Option<&'a Arc<ColumnInfo>> {
    let name = tidb_base::gostd::to_lower(name);
    for col in cols {
        let col = col.expect("invalid memory address or nil pointer dereference");
        if col.name.l == name {
            return Some(col);
        }
    }
    None
}

impl Clone for TableInfo {
    /// Clone clones TableInfo: columns and indices are cloned (nil becomes empty), foreign
    /// keys only if there are any (otherwise nil), partition, TTL and affinity info cloned.
    // go: TableInfo.Clone L1
    fn clone(&self) -> TableInfo {
        let mut nt = self.struct_copy();
        // Go clones each column and index (`t.Columns[i].Clone()`): new values, not shared.
        nt.columns = Some(
            sl(&self.columns)
                .iter()
                .map(|c| Arc::new(c.as_ref().clone()))
                .collect(),
        );
        nt.indices = Some(
            sl(&self.indices)
                .iter()
                .map(|i| Arc::new(i.as_ref().clone()))
                .collect(),
        );
        nt.foreign_keys = None;
        if !sl(&self.foreign_keys).is_empty() {
            nt.foreign_keys = Some(sl(&self.foreign_keys).to_vec());
        }
        if let Some(p) = &self.partition {
            nt.partition = Some(Box::new(p.as_ref().clone()));
        }
        if let Some(t) = &self.ttl_info {
            nt.ttl_info = Some(Box::new(t.as_ref().clone()));
        }
        if let Some(a) = &self.affinity {
            nt.affinity = Some(Box::new(a.as_ref().clone()));
        }
        nt
    }
}

/// FindFKInfoByName finds FKInfo in fks by lowercase name.
// go: FindFKInfoByName L1
pub fn find_fk_info_by_name<'a>(fks: &'a [FKInfo], name: &str) -> Option<&'a FKInfo> {
    fks.iter().find(|fk| fk.name.l == name)
}

impl TableCacheStatusType {
    /// String implements fmt.Stringer interface.
    // go: TableCacheStatusType.String L1 [M-NAME]
    pub fn to_string(self) -> &'static str {
        match self {
            TableCacheStatusDisable => "disable",
            TableCacheStatusEnable => "enable",
            TableCacheStatusSwitching => "switching",
            _ => "",
        }
    }
}

impl TempTableType {
    /// String implements fmt.Stringer interface.
    // go: TempTableType.String L1 [M-NAME]
    pub fn to_string(self) -> &'static str {
        match self {
            TempTableGlobal => "global",
            TempTableLocal => "local",
            _ => "",
        }
    }
}

impl SessionInfo {
    /// String implements fmt.Stringer interface.
    // go: SessionInfo.String L1 [M-NAME]
    #[allow(clippy::inherent_to_string)]
    pub fn to_string(&self) -> String {
        format!("server: {}_session: {}", self.server_id, self.session_id)
    }
}

impl TableLockState {
    /// String implements fmt.Stringer interface.
    // go: TableLockState.String L1 [M-NAME]
    pub fn to_string(self) -> &'static str {
        match self {
            TableLockStatePreLock => "pre-lock",
            TableLockStatePublic => "public",
            _ => "none",
        }
    }
}

impl TableMode {
    /// String implements fmt.Stringer interface.
    // go: TableMode.String L1 [M-NAME]
    pub fn to_string(self) -> &'static str {
        match self {
            TableModeNormal => "Normal",
            TableModeImport => "Import",
            TableModeRestore => "Restore",
            _ => "",
        }
    }
}

impl TiFlashReplicaInfo {
    /// IsPartitionAvailable checks whether the partition table replica was available.
    // go: TiFlashReplicaInfo.IsPartitionAvailable L1
    pub fn is_partition_available(&self, pid: i64) -> bool {
        sl(&self.available_partition_ids).contains(&pid)
    }
}

impl Clone for PartitionInfo {
    /// Clone clones itself: columns and the three definition lists are cloned (nil becomes
    /// empty).
    // go: PartitionInfo.Clone L1
    fn clone(&self) -> PartitionInfo {
        let mut new_pi = self.struct_copy();
        new_pi.columns = Some(sl(&self.columns).to_vec());
        new_pi.definitions = Some(sl(&self.definitions).to_vec());
        new_pi.adding_definitions = Some(sl(&self.adding_definitions).to_vec());
        new_pi.dropping_definitions = Some(sl(&self.dropping_definitions).to_vec());
        new_pi
    }
}

impl PartitionInfo {
    /// GetNameByID gets the partition name by ID.
    // go: PartitionInfo.GetNameByID L1
    pub fn get_name_by_id(&self, id: i64) -> &str {
        sl(&self.definitions)
            .iter()
            .find(|d| d.id == id)
            .map_or("", |d| d.name.o.as_str())
    }

    /// GetStateByID gets the partition state by ID.
    // go: PartitionInfo.GetStateByID L1
    pub fn get_state_by_id(&self, id: i64) -> SchemaState {
        sl(&self.states)
            .iter()
            .find(|s| s.id == id)
            .map_or(StatePublic, |s| s.state)
    }

    /// SetStateByID sets the state of the partition by ID.
    // go: PartitionInfo.SetStateByID L1
    pub fn set_state_by_id(&mut self, id: i64, state: SchemaState) {
        let new_state = PartitionState { id, state };
        if let Some(states) = &mut self.states {
            for pstate in states.iter_mut() {
                if pstate.id == id {
                    *pstate = new_state;
                    return;
                }
            }
        }
        self.states.get_or_insert_with(Vec::new).push(new_state);
    }

    /// GCPartitionStates cleans up the partition state.
    // go: PartitionInfo.GCPartitionStates L1
    pub fn gc_partition_states(&mut self) {
        if sl(&self.states).is_empty() {
            return;
        }
        let defs = sl(&self.definitions);
        let new_states: Vec<PartitionState> = sl(&self.states)
            .iter()
            .filter(|state| defs.iter().any(|def| def.id == state.id))
            .cloned()
            .collect();
        self.states = Some(new_states);
    }

    /// ClearReorgIntermediateInfo remove intermediate information used during reorganize
    /// partition.
    // go: PartitionInfo.ClearReorgIntermediateInfo L1
    pub fn clear_reorg_intermediate_info(&mut self) {
        self.ddl_action = ActionNone;
        self.ddl_state = StateNone;
        self.ddl_type = PartitionTypeNone;
        self.ddl_expr = String::new();
        self.ddl_columns = None;
        self.new_table_id = 0;
        self.ddl_changed_index = None;
    }

    /// FindPartitionDefinitionByName finds PartitionDefinition by name.
    // go: PartitionInfo.FindPartitionDefinitionByName L1
    pub fn find_partition_definition_by_name(&self, partition_definition_name: &str) -> isize {
        let low_constr_name = tidb_base::gostd::to_lower(partition_definition_name);
        sl(&self.definitions)
            .iter()
            .position(|d| d.name.l == low_constr_name)
            .map_or(-1, |i| i as isize)
    }

    /// GetPartitionIDByName gets the partition ID by name.
    // go: PartitionInfo.GetPartitionIDByName L1
    pub fn get_partition_id_by_name(&self, partition_definition_name: &str) -> i64 {
        let low_constr_name = tidb_base::gostd::to_lower(partition_definition_name);
        sl(&self.definitions)
            .iter()
            .find(|d| d.name.l == low_constr_name)
            .map_or(-1, |d| d.id)
    }

    /// GetDefaultListPartition return the index of Definitions that contains the LIST
    /// Default partition otherwise it returns -1.
    // go: PartitionInfo.GetDefaultListPartition L1
    pub fn get_default_list_partition(&self) -> isize {
        if self.type_ != PartitionTypeList {
            return -1;
        }
        for (i, def) in sl(&self.definitions).iter().enumerate() {
            if sl(&def.in_values).is_empty() {
                return i as isize;
            }
            for vs in sl(&def.in_values) {
                let vs = sl(vs);
                if vs.len() == 1 && vs[0] == "DEFAULT" {
                    return i as isize;
                }
            }
        }
        -1
    }

    /// CanHaveOverlappingDroppingPartition returns true if special handling is needed
    /// during DDL of partitioned tables, where range or list with default partition can have
    /// overlapping partitions.
    // go: PartitionInfo.CanHaveOverlappingDroppingPartition L1
    pub fn can_have_overlapping_dropping_partition(&self) -> bool {
        self.ddl_action == ActionDropTablePartition && self.ddl_state == StateWriteOnly
    }

    /// ReplaceWithOverlappingPartitionIdx returns the overlapping partition if there is
    /// one and a previous error.
    // go: PartitionInfo.ReplaceWithOverlappingPartitionIdx L1 [M-ERRVAL]
    pub fn replace_with_overlapping_partition_idx(
        &self,
        mut idx: isize,
        mut err: Option<Error>,
    ) -> (isize, Option<Error>) {
        if err.is_some() && idx >= 0 {
            idx = self.get_overlapping_dropping_partition_idx(idx);
            if idx >= 0 {
                err = None;
            }
        }
        (idx, err)
    }

    /// GetOverlappingDroppingPartitionIdx takes the index of Definitions and returns
    /// possible overlapping partition to use instead.
    // go: PartitionInfo.GetOverlappingDroppingPartitionIdx L1
    pub fn get_overlapping_dropping_partition_idx(&self, idx: isize) -> isize {
        let n = sl(&self.definitions).len() as isize;
        if idx < 0 || idx >= n {
            return -1;
        }
        if self.can_have_overlapping_dropping_partition() {
            if self.type_ == PartitionTypeRange {
                for i in idx..n {
                    if self.is_dropping(i) {
                        continue;
                    }
                    return i;
                }
                // Last partition is also dropped!
                return -1;
            } else if self.type_ == PartitionTypeList {
                if self.is_dropping(idx) {
                    let default_idx = self.get_default_list_partition();
                    if default_idx == idx {
                        return -1;
                    }
                    return default_idx;
                }
                return idx;
            }
        }
        idx
    }

    /// IsDropping returns true if the partition is being dropped (i.e. in
    /// DroppingDefinitions). Panics if `idx` is out of range and there are dropping
    /// definitions, as Go does.
    // go: PartitionInfo.IsDropping L1
    pub fn is_dropping(&self, idx: isize) -> bool {
        for def in sl(&self.dropping_definitions) {
            if def.id == sl(&self.definitions)[usize::try_from(idx).unwrap_or(usize::MAX)].id {
                return true;
            }
        }
        false
    }

    /// SetOriginalPartitionIDs sets the order of the original partition IDs in case it
    /// needs to be rolled back.
    // go: PartitionInfo.SetOriginalPartitionIDs L1
    pub fn set_original_partition_ids(&mut self) {
        let ids: Vec<i64> = sl(&self.definitions).iter().map(|d| d.id).collect();
        self.original_partition_ids_order = Some(ids);
    }

    /// IDsInDDLToIgnore returns a list of IDs that the current session should not see (may
    /// be duplicate errors on insert/update though). For example during truncate or drop
    /// partition.
    // go: PartitionInfo.IDsInDDLToIgnore L1
    pub fn ids_in_ddl_to_ignore(&self) -> Option<Vec<i64>> {
        let ids_of =
            |defs: &[PartitionDefinition]| -> Vec<i64> { defs.iter().map(|d| d.id).collect() };
        if self.ddl_action == ActionTruncateTablePartition {
            if self.ddl_state == StateWriteOnly {
                return self.new_partition_ids.clone();
            }
            if self.ddl_state == StateDeleteOnly || self.ddl_state == StateDeleteReorganization {
                if sl(&self.dropping_definitions).is_empty() {
                    return None;
                }
                return Some(ids_of(sl(&self.dropping_definitions)));
            }
        } else if self.ddl_action == ActionDropTablePartition {
            if !sl(&self.dropping_definitions).is_empty() && self.ddl_state == StateDeleteOnly {
                return Some(ids_of(sl(&self.dropping_definitions)));
            }
        } else if self.ddl_action == ActionAddTablePartition
            && !sl(&self.adding_definitions).is_empty()
        {
            return Some(ids_of(sl(&self.adding_definitions)));
        }
        None
    }
}

impl Clone for PartitionDefinition {
    /// Clone clones the definition: `LessThan` is copied (nil becomes empty).
    // go: PartitionDefinition.Clone L1
    fn clone(&self) -> PartitionDefinition {
        let mut nci = self.struct_copy();
        nci.less_than = Some(sl(&self.less_than).to_vec());
        nci
    }
}

impl PartitionDefinition {
    /// MemoryUsage return the memory usage of PartitionDefinition.
    // go: PartitionDefinition.MemoryUsage L1
    pub fn memory_usage(&self) -> i64 {
        let mut sum = emptyPartitionDefinitionSize + self.name.memory_usage();
        if let Some(p) = &self.placement_policy_ref {
            sum += 8 + p.name.memory_usage();
        }
        for s in sl(&self.less_than) {
            sum += s.len() as i64;
        }
        for strs in sl(&self.in_values) {
            for s in sl(strs) {
                sum += s.len() as i64;
            }
        }
        sum
    }
}

impl Clone for ConstraintInfo {
    /// Clone clones ConstraintInfo (the constraint columns are copied; nil becomes empty).
    // go: ConstraintInfo.Clone L1
    fn clone(&self) -> ConstraintInfo {
        let mut nci = self.struct_copy();
        nci.constraint_cols = Some(sl(&self.constraint_cols).to_vec());
        nci
    }
}

impl FKInfo {
    /// String returns the string of the foreign key.
    // go: FKInfo.String L1 [M-NAME]
    pub fn to_string(&self, db: &str, tb: &str) -> String {
        let mut buf = String::new();
        buf.push_str(&format!("`{db}`.`"));
        buf.push_str(&format!("{tb}`, CONSTRAINT `"));
        buf.push_str(&format!("{}` FOREIGN KEY (", self.name.o));
        for (i, col) in sl(&self.cols).iter().enumerate() {
            if i > 0 {
                buf.push_str(", ");
            }
            buf.push_str(&format!("`{}`", col.o));
        }
        buf.push_str(") REFERENCES `");
        if self.ref_schema.l != db {
            buf.push_str(&self.ref_schema.l);
            buf.push_str("`.`");
        }
        buf.push_str(&self.ref_table.l);
        buf.push_str("` (");
        for (i, col) in sl(&self.ref_cols).iter().enumerate() {
            if i > 0 {
                buf.push_str(", ");
            }
            buf.push_str(&format!("`{}`", col.o));
        }
        buf.push(')');
        let on_delete = ReferOptionType(self.on_delete);
        if on_delete != ReferOptionNoOption {
            buf.push_str(" ON DELETE ");
            buf.push_str(on_delete.to_string());
        }
        let on_update = ReferOptionType(self.on_update);
        if on_update != ReferOptionNoOption {
            buf.push_str(" ON UPDATE ");
            buf.push_str(on_update.to_string());
        }
        buf
    }
}

impl Clone for FKInfo {
    /// Clone clones FKInfo (the column lists are copied; nil becomes empty).
    // go: FKInfo.Clone L1
    fn clone(&self) -> FKInfo {
        let mut nfk = self.struct_copy();
        nfk.ref_cols = Some(sl(&self.ref_cols).to_vec());
        nfk.cols = Some(sl(&self.cols).to_vec());
        nfk
    }
}

impl TableItemID {
    /// Key is used to generate unique key for TableItemID to use in the syncload.
    // go: TableItemID.Key L1
    pub fn key(&self) -> String {
        format!("{}#{}#{}", self.id, self.table_id, self.is_index)
    }
}

impl StatsLoadItem {
    /// Key is used to generate unique key for TableItemID to use in the syncload.
    // go: StatsLoadItem.Key L1
    pub fn key(&self) -> String {
        format!("{}#{}", self.table_item_id.key(), self.full_load)
    }
}

/// NewStatsOptions creates a new StatsOptions.
// go: NewStatsOptions L1
pub fn new_stats_options() -> StatsOptions {
    StatsOptions {
        auto_recalc: true,
        column_choice: tidb_parser::model::DefaultChoice,
        column_list: Some(Vec::new()),
        sample_num: 0,
        sample_rate: 0.0,
        buckets: 0,
        top_n: 0,
        concurrency: 0,
        ..StatsOptions::default()
    }
}

impl WindowRepeatType {
    /// String implements fmt.Stringer interface.
    // go: WindowRepeatType.String L1 [M-NAME]
    pub fn to_string(self) -> &'static str {
        match self {
            Never => "Never",
            Day => "Day",
            Week => "Week",
            Month => "Month",
            _ => "",
        }
    }
}

impl Clone for TTLInfo {
    /// Clone clones TTLInfo (a struct copy).
    // go: TTLInfo.Clone L1
    fn clone(&self) -> TTLInfo {
        self.struct_copy()
    }
}

impl TTLInfo {
    /// GetJobInterval parses the job interval and return. If the job interval is an empty
    /// string, the "1h" will be returned, to keep compatible with 6.5 (in which job interval
    /// doesn't exist).
    // go: TTLInfo.GetJobInterval L1 [M-ERR]
    pub fn get_job_interval(&self) -> Result<Duration, Error> {
        if self.job_interval.is_empty() {
            return Ok(DefaultJobInterval);
        }
        tidb_parser::duration::parse_duration(&self.job_interval)
    }
}

/// NewTableAffinityInfoWithLevel creates a new TableAffinityInfo with the given level; the
/// level `none` (or empty) gives `None`.
// go: NewTableAffinityInfoWithLevel L1 [M-ERR]
pub fn new_table_affinity_info_with_level(level: &str) -> Result<Option<TableAffinityInfo>, Error> {
    let (normalized, ok) = tidb_parser::ast_ddl::normalize_table_affinity_level(level);
    if !ok {
        return Err(errors::errorf(
            "invalid table affinity level: '%s'",
            &[Arg::from(level.to_owned())],
        ));
    }
    if normalized == tidb_parser::ast_ddl::TableAffinityLevelNone {
        return Ok(None);
    }
    Ok(Some(TableAffinityInfo { level: normalized }))
}

impl Clone for TableAffinityInfo {
    /// Clone clones TableAffinityInfo (a struct copy).
    // go: TableAffinityInfo.Clone L1
    fn clone(&self) -> TableAffinityInfo {
        self.struct_copy()
    }
}

impl Clone for SoftdeleteInfo {
    /// Clone clones SoftdeleteInfo (a struct copy).
    // go: SoftdeleteInfo.Clone L1
    fn clone(&self) -> SoftdeleteInfo {
        self.struct_copy()
    }
}

/// TableNameInfo provides meta data describing a table name info.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TableNameInfo {
    pub id: i64,
    pub name: CIStr,
}

/// ReferredFKInfo provides the cited foreign key in the child table.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ReferredFKInfo {
    pub cols: Option<Vec<CIStr>>,
    pub child_schema: CIStr,
    pub child_table: CIStr,
    pub child_fk_name: CIStr,
}
