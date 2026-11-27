//! go-file: pkg/planner/core/access_object.go
//!
//! The access objects of the operators of the first vertical slice (block D10.9): what a
//! table scan, a point get and a table reader read, for the "access object" column of
//! EXPLAIN and the binary plan.
//!
//! - `base.AccessObject` is the trait [`crate::base::AccessObject`]; Go's `String()` of an
//!   implementation is its `to_string`, and of `DynamicPartitionAccessObject` (a Stringer
//!   only) its `Display` (`M-NAME`).
//! - Go's nil `*tipb.ExplainOperator` is `None`.
//! - Partitioned tables stop in the plan builder (`blocked-by:D09.12`): the dynamic pruning
//!   access object (`getDynamicAccessPartition`) returns a blocked error where Go would run
//!   `PartitionPruning`, so `PhysicalTableReader.accessObject` returns a `Result`.
//! - `PhysicalTableScan.AnnIndexExtra` (vector indexes) and
//!   `PhysicalTableReader.TableScanAndPartitionInfos` (MPP) are not carried (D10.1 spec §3):
//!   their branches see Go's nil / empty value.

use std::fmt;

use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_model::TableInfo;
use tipb::ExplainOperator;

use crate::base::AccessObject;
use crate::core::physical_plans::{
    PhysPlanPartInfo, PhysicalIndexReader, PhysicalIndexScan, PhysicalTableReader,
    PhysicalTableScan,
};
use crate::core::point_get_plan::PointGetPlan;
use crate::planctx::PlanCtx;

/// DynamicPartitionAccessObject represents the partitions accessed by the children of this
/// operator. It's mainly used in dynamic pruning mode.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DynamicPartitionAccessObject {
    /// Database.
    pub database: String,
    /// Table.
    pub table: String,
    /// AllPartitions.
    pub all_partitions: bool,
    /// Partitions.
    pub partitions: Vec<String>,
    /// err.
    pub err: String,
}

impl fmt::Display for DynamicPartitionAccessObject {
    // go: DynamicPartitionAccessObject.String L1 [M-NAME]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if !self.err.is_empty() {
            return f.write_str(&self.err);
        }
        if self.all_partitions {
            return f.write_str("partition:all");
        } else if self.partitions.is_empty() {
            return f.write_str("partition:dual");
        }
        write!(f, "partition:{}", self.partitions.join(","))
    }
}

/// DynamicPartitionAccessObjects is a list of DynamicPartitionAccessObject.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DynamicPartitionAccessObjects(pub Vec<DynamicPartitionAccessObject>);

impl AccessObject for DynamicPartitionAccessObjects {
    // go: DynamicPartitionAccessObjects.String L1
    fn to_string(&self) -> String {
        let d = &self.0;
        if d.is_empty() {
            return String::new();
        }
        if d.len() == 1 {
            return d[0].to_string();
        }
        let mut b = String::new();
        for (i, access) in d.iter().enumerate() {
            if i != 0 {
                b.push_str(", ");
            }
            b.push_str(&access.to_string());
            b.push_str(&format!(" of {}", access.table));
        }
        b
    }

    /// NormalizedString implements AccessObject.
    // go: DynamicPartitionAccessObjects.NormalizedString L1
    fn normalized_string(&self) -> String {
        AccessObject::to_string(self)
    }

    /// SetIntoPB implements AccessObject.
    // go: DynamicPartitionAccessObjects.SetIntoPB L1
    fn set_into_pb(&self, pb: Option<&mut ExplainOperator>) {
        let d = &self.0;
        let Some(pb) = pb else { return };
        if d.is_empty() {
            return;
        }
        let mut pb_obj_slice = vec![tipb::DynamicPartitionAccessObject::default(); d.len()];
        for (i, obj) in d.iter().enumerate() {
            if !obj.err.is_empty() {
                continue;
            }
            let pb_obj = &mut pb_obj_slice[i];
            pb_obj.set_database(obj.database.clone());
            pb_obj.set_table(obj.table.clone());
            pb_obj.set_all_partitions(obj.all_partitions);
            pb_obj.set_partitions(obj.partitions.clone().into());
        }
        let mut pb_objs = tipb::DynamicPartitionAccessObjects::default();
        pb_objs.set_objects(pb_obj_slice.into());
        let mut access_object = tipb::AccessObject::default();
        access_object.set_dynamic_partition_objects(pb_objs);
        pb.set_access_objects(vec![access_object].into());
    }
}

/// IndexAccess represents the index accessed by an operator.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexAccess {
    /// Name.
    pub name: String,
    /// Cols.
    pub cols: Vec<String>,
    /// IsClusteredIndex.
    pub is_clustered_index: bool,
}

impl IndexAccess {
    /// ToPB turns itself into a protobuf message. Go's nil receiver does not occur: the
    /// only caller passes `&s.Indexes[i]`.
    // go: IndexAccess.ToPB L1
    pub fn to_pb(&self) -> tipb::IndexAccess {
        let mut pb = tipb::IndexAccess::default();
        pb.set_name(self.name.clone());
        pb.set_cols(self.cols.clone().into());
        pb.set_is_clustered_index(self.is_clustered_index);
        pb
    }
}

/// ScanAccessObject represents the access to a table. It may also represent the access to
/// indexes and partitions of a table.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanAccessObject {
    /// Database.
    pub database: String,
    /// Table.
    pub table: String,
    /// Indexes.
    pub indexes: Vec<IndexAccess>,
    /// Partitions.
    pub partitions: Vec<String>,
}

impl AccessObject for ScanAccessObject {
    /// NormalizedString implements AccessObject.
    // go: ScanAccessObject.NormalizedString L1
    fn normalized_string(&self) -> String {
        let mut b = String::new();
        if !self.table.is_empty() {
            b.push_str(&format!("table:{}", self.table));
        }
        if !self.partitions.is_empty() {
            b.push_str(", partition:?");
        }
        for index in &self.indexes {
            if index.is_clustered_index {
                b.push_str(", clustered index:");
            } else {
                b.push_str(", index:");
            }
            b.push_str(&format!("{}({})", index.name, index.cols.join(", ")));
        }
        b
    }

    // go: ScanAccessObject.String L1
    fn to_string(&self) -> String {
        let mut b = String::new();
        if !self.table.is_empty() {
            b.push_str(&format!("table:{}", self.table));
        }
        if !self.partitions.is_empty() {
            b.push_str(&format!(", partition:{}", self.partitions.join(",")));
        }
        for index in &self.indexes {
            if index.is_clustered_index {
                b.push_str(", clustered index:");
            } else {
                b.push_str(", index:");
            }
            b.push_str(&format!("{}({})", index.name, index.cols.join(", ")));
        }
        b
    }

    /// SetIntoPB implements AccessObject.
    // go: ScanAccessObject.SetIntoPB L1
    fn set_into_pb(&self, pb: Option<&mut ExplainOperator>) {
        let Some(pb) = pb else { return };
        let mut pb_obj = tipb::ScanAccessObject::default();
        pb_obj.set_database(self.database.clone());
        pb_obj.set_table(self.table.clone());
        pb_obj.set_partitions(self.partitions.clone().into());
        for index in &self.indexes {
            pb_obj.mut_indexes().push(index.to_pb());
        }
        let mut access_object = tipb::AccessObject::default();
        access_object.set_scan_object(pb_obj);
        pb.set_access_objects(vec![access_object].into());
    }
}

/// OtherAccessObject represents other kinds of access.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OtherAccessObject(pub String);

impl AccessObject for OtherAccessObject {
    // go: OtherAccessObject.String L1
    fn to_string(&self) -> String {
        self.0.clone()
    }

    /// NormalizedString implements AccessObject.
    // go: OtherAccessObject.NormalizedString L1
    fn normalized_string(&self) -> String {
        AccessObject::to_string(self)
    }

    /// SetIntoPB implements AccessObject.
    // go: OtherAccessObject.SetIntoPB L1
    fn set_into_pb(&self, pb: Option<&mut ExplainOperator>) {
        let Some(pb) = pb else { return };
        if self.0.is_empty() {
            return;
        }
        let mut access_object = tipb::AccessObject::default();
        access_object.set_other_object(self.0.clone());
        pb.set_access_objects(vec![access_object].into());
    }
}

/// `p.Table.Columns[idxCol.Offset]`, as the index access of a hidden column shows its
/// expression; Go panics on a missing column, so does the indexing.
// glue: the column loop shared by the AccessObject methods (inlined in Go)
fn index_access_cols(tbl: &TableInfo, index_cols: &[tidb_model::IndexColumn]) -> Vec<String> {
    let columns = tbl.columns.as_deref().unwrap_or(&[]);
    let mut cols = Vec::new();
    for idx_col in index_cols {
        let tbl_col = &columns[idx_col.offset as usize];
        if tbl_col.hidden {
            cols.push(tbl_col.generated_expr_string.clone());
        } else {
            cols.push(idx_col.name.o.clone());
        }
    }
    cols
}

impl PhysicalIndexScan {
    /// AccessObject implements dataAccesser interface.
    // go: PhysicalIndexScan.AccessObject L1
    pub fn access_object(&self) -> Box<dyn AccessObject> {
        let mut res = ScanAccessObject {
            database: self.db_name.o.clone(),
            ..Default::default()
        };
        let mut tbl_name = self.table.name.o.clone();
        if let Some(as_name) = &self.table_as_name
            && !as_name.o.is_empty()
        {
            tbl_name = as_name.o.clone();
        }
        res.table = tbl_name;
        if self.is_partition
            && let Some(pi) = self.table.get_partition_info()
        {
            let partition_name = pi.get_name_by_id(self.physical_table_id);
            res.partitions = vec![partition_name.to_owned()];
        }
        let idx_cols = self.index.columns.as_deref().unwrap_or(&[]);
        if !idx_cols.is_empty() {
            let index = IndexAccess {
                name: self.index.name.o.clone(),
                cols: index_access_cols(&self.table, idx_cols),
                is_clustered_index: false,
            };
            res.indexes = vec![index];
        }
        Box::new(res)
    }
}

impl PhysicalTableScan {
    /// AccessObject implements dataAccesser interface.
    // go: PhysicalTableScan.AccessObject L2 (AnnIndexExtra is not carried: its branch sees nil)
    pub fn access_object(&self) -> Box<dyn AccessObject> {
        let mut res = ScanAccessObject {
            database: self.db_name.o.clone(),
            ..Default::default()
        };
        let mut tbl_name = self.table.name.o.clone();
        if let Some(as_name) = &self.table_as_name
            && !as_name.o.is_empty()
        {
            tbl_name = as_name.o.clone();
        }
        res.table = tbl_name;
        if self.is_partition
            && let Some(pi) = self.table.get_partition_info()
        {
            let partition_name = pi.get_name_by_id(self.physical_table_id);
            res.partitions = vec![partition_name.to_owned()];
        }
        // p.AnnIndexExtra is nil (not carried, module doc).
        Box::new(res)
    }
}

impl PointGetPlan {
    /// AccessObject implements dataAccesser interface.
    // go: PointGetPlan.AccessObject L1
    pub fn access_object(&self) -> Box<dyn AccessObject> {
        let mut res = ScanAccessObject {
            database: self.db_name.clone(),
            table: self.tbl_info.name.o.clone(),
            ..Default::default()
        };
        if let Some(idx) = self.partition_idx {
            if idx < 0 {
                res.partitions = vec!["dual".to_owned()];
            } else if let Some(pi) = self.tbl_info.get_partition_info() {
                let definitions = pi.definitions.as_deref().unwrap_or(&[]);
                res.partitions = vec![definitions[idx as usize].name.o.clone()];
            }
        }
        if let Some(index_info) = &self.index_info {
            let index = IndexAccess {
                name: index_info.name.o.clone(),
                is_clustered_index: index_info.primary && self.tbl_info.is_common_handle,
                cols: index_access_cols(
                    &self.tbl_info,
                    index_info.columns.as_deref().unwrap_or(&[]),
                ),
            };
            res.indexes = vec![index];
        }
        Box::new(res)
    }
}

/// `getDynamicAccessPartition`. Go returns nil for a table that is not partitioned or when
/// the statement does not use dynamic pruning (`Ok(None)`); for a partitioned table it runs
/// `PartitionPruning`, which is not ported: partitioned tables stop in the plan builder.
// go: getDynamicAccessPartition L2 (the partition pruning part is blocked-by:D09.12)
pub fn get_dynamic_access_partition(
    sctx: &PlanCtx<'_>,
    tbl_info: &TableInfo,
    _phys_plan_part_info: Option<&PhysPlanPartInfo>,
    _as_name: &str,
) -> Result<Option<DynamicPartitionAccessObject>, Error> {
    let pi = tbl_info.get_partition_info();
    if pi.is_none() || !sctx.vars.stmt_ctx().use_dynamic_partition_prune() {
        return Ok(None);
    }
    // glue: blocked-by:D09.12 (the access object of a partitioned table needs PartitionPruning)
    Err(blocked(
        "D09.12",
        "the partitions a partitioned table reader accesses (PartitionPruning)",
    ))
}

impl PhysicalTableReader {
    /// `accessObject` of `partitionAccesser`. Returns a `Result` for the partition pruning
    /// boundary of [`get_dynamic_access_partition`]; `TableScanAndPartitionInfos` is not
    /// carried, so only Go's `len(...) == 0` branch is reached.
    // go: PhysicalTableReader.accessObject L2 (Result for the boundary; TableScanAndPartitionInfos is not carried)
    pub fn access_object(&self, sctx: &PlanCtx<'_>) -> Result<Box<dyn AccessObject>, Error> {
        if !sctx.vars.stmt_ctx().use_dynamic_partition_prune() {
            return Ok(Box::new(DynamicPartitionAccessObjects(Vec::new())));
        }
        // len(p.TableScanAndPartitionInfos) == 0
        let Some(ts) = self.table_plans[0]
            .as_any()
            .downcast_ref::<PhysicalTableScan>()
        else {
            return Ok(Box::new(OtherAccessObject(String::new())));
        };
        let mut as_name = "";
        if let Some(n) = &ts.table_as_name
            && !n.o.is_empty()
        {
            as_name = &n.o;
        }
        let res =
            get_dynamic_access_partition(sctx, &ts.table, self.plan_part_info.as_deref(), as_name)?;
        match res {
            None => Ok(Box::new(DynamicPartitionAccessObjects(Vec::new()))),
            Some(res) => Ok(Box::new(DynamicPartitionAccessObjects(vec![res]))),
        }
    }
}

// go: getAccessObjectFromIndexScan L2 (Result for the partition pruning boundary of getDynamicAccessPartition)
pub fn get_access_object_from_index_scan(
    sctx: &PlanCtx<'_>,
    is: &PhysicalIndexScan,
    p: Option<&PhysPlanPartInfo>,
) -> Result<Box<dyn AccessObject>, Error> {
    if !sctx.vars.stmt_ctx().use_dynamic_partition_prune() {
        return Ok(Box::new(DynamicPartitionAccessObjects(Vec::new())));
    }
    let mut as_name = "";
    if let Some(n) = &is.table_as_name
        && !n.o.is_empty()
    {
        as_name = &n.o;
    }
    let res = get_dynamic_access_partition(sctx, &is.table, p, as_name)?;
    match res {
        None => Ok(Box::new(DynamicPartitionAccessObjects(Vec::new()))),
        Some(res) => Ok(Box::new(DynamicPartitionAccessObjects(vec![res]))),
    }
}

impl PhysicalIndexReader {
    /// `accessObject` of `partitionAccesser`.
    // go: PhysicalIndexReader.accessObject L2 (Result for the partition pruning boundary)
    pub fn access_object(&self, sctx: &PlanCtx<'_>) -> Result<Box<dyn AccessObject>, Error> {
        let is = self.index_plans[0]
            .as_any()
            .downcast_ref::<PhysicalIndexScan>()
            .expect("interface conversion: base.PhysicalPlan is not *core.PhysicalIndexScan");
        get_access_object_from_index_scan(sctx, is, self.plan_part_info.as_deref())
    }
}

impl crate::core::physical_plans::PhysicalIndexLookUpReader {
    /// `accessObject` of `partitionAccesser`.
    // go: PhysicalIndexLookUpReader.accessObject L2 (Result for the partition pruning boundary)
    pub fn access_object(&self, sctx: &PlanCtx<'_>) -> Result<Box<dyn AccessObject>, Error> {
        let is = self.index_plans[0]
            .as_any()
            .downcast_ref::<PhysicalIndexScan>()
            .expect("interface conversion: base.PhysicalPlan is not *core.PhysicalIndexScan");
        get_access_object_from_index_scan(sctx, is, self.plan_part_info.as_deref())
    }
}

#[cfg(test)]
mod tests;
