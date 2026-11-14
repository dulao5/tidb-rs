//! go-file: pkg/planner/property/physical_property.go
//!
//! Mapping:
//! - `*expression.Column` is `Arc<Column>` (D08.1); `Clone` of a column shares the `Arc`
//!   (`M-EXPR`: columns are immutable once built, a writer copies first).
//! - `PhysicalProperty.hashcode` is computed lazily by `HashCode()` behind a pointer receiver;
//!   here it is a `OnceLock` so the read path stays `&self` (N1 §3.7) and the property can be
//!   shared between threads with the physical plan. Go's capacity of the buffer is tracked
//!   next to it, because `MemoryUsage` adds `cap(p.hashcode)` (Go's `append` growth,
//!   `growslice_cap`).
//! - `SortItem.Hash64` / `SortItem.Equals` implement cascades' `base.HashEquals`
//!   (`pkg/planner/cascades/base` is EXC.B, not ported; as `Column.Hash64` in D08.1 spec §4
//!   item 2).
//! - `VectorProp` keeps Go's shape (`*expression.VectorHelper` and `TopK`); the vector search
//!   it serves is TiFlash only (exception D1), nothing sets it in VS1.

#![allow(non_upper_case_globals)]

use std::sync::{Arc, OnceLock};

use tidb_base::gostd::runtime::growslice_cap;
use tidb_codec::{encode_float, encode_int};
use tidb_expression::builtin_vec::VectorHelper;
use tidb_expression::{Column, EvalContext, Schema};
use tidb_types::collate;

use super::task_type::{
    CopMultiReadTaskType, CopSingleReadTaskType, MppTaskType, RootTaskType, TaskType,
};

/// wholeTaskTypes records all possible kinds of task that a plan can return. For Agg, TopN and Limit, we will try to get
/// these tasks one by one.
const wholeTaskTypes: [TaskType; 3] = [CopSingleReadTaskType, CopMultiReadTaskType, RootTaskType];

/// `size.SizeOfBool`.
const SizeOfBool: i64 = 1;
/// `size.SizeOfInt32`.
const SizeOfInt32: i64 = 4;

// go-exception: EXC.B SortItem.Hash64 (cascades base.HashEquals; D08.1 spec §4 item 2)
// go-exception: EXC.B SortItem.Equals (cascades base.HashEquals; D08.1 spec §4 item 2)

/// SortItem wraps the column and its order.
#[derive(Clone, Debug)]
pub struct SortItem {
    /// Col.
    pub col: Arc<Column>,
    /// Desc.
    pub desc: bool,
}

impl SortItem {
    /// `(*SortItem).String`. Go formats the column with its `String` method.
    // go: SortItem.String L1 [M-NAME]
    pub fn to_string(&self) -> Vec<u8> {
        let mut out = b"{".to_vec();
        out.extend(self.col.to_string());
        out.extend_from_slice(if self.desc { b" desc}" } else { b" asc}" });
        out
    }

    /// Clone makes a copy of SortItem.
    // go: SortItem.Clone L1 [M-EXPR]
    pub fn clone_item(&self) -> SortItem {
        SortItem {
            col: Arc::clone(&self.col),
            desc: self.desc,
        }
    }

    /// MemoryUsage return the memory usage of SortItem
    // go: SortItem.MemoryUsage L1
    pub fn memory_usage(&self) -> i64 {
        SizeOfBool + self.col.memory_usage()
    }
}

/// MPPPartitionType is the way to partition during mpp data exchanging.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct MPPPartitionType(pub isize);

/// AnyType will not require any special partition types.
pub const AnyType: MPPPartitionType = MPPPartitionType(0);
/// BroadcastType requires current task to broadcast its data.
pub const BroadcastType: MPPPartitionType = MPPPartitionType(1);
/// HashType requires current task to shuffle its data according to some columns.
pub const HashType: MPPPartitionType = MPPPartitionType(2);
/// SinglePartitionType requires all the task pass the data to one node (tidb/tiflash).
pub const SinglePartitionType: MPPPartitionType = MPPPartitionType(3);

impl MPPPartitionType {
    /// ToExchangeType generates ExchangeType from MPPPartitionType
    // go: MPPPartitionType.ToExchangeType L1
    pub fn to_exchange_type(self) -> tipb::ExchangeType {
        match self {
            BroadcastType => tipb::ExchangeType::Broadcast,
            HashType => tipb::ExchangeType::Hash,
            SinglePartitionType => tipb::ExchangeType::PassThrough,
            _ => {
                log::warn!("generate an exchange with any partition type, which is illegal.");
                tipb::ExchangeType::PassThrough
            }
        }
    }
}

/// MPPPartitionColumn is the column that will be used in MPP Hash Exchange
#[derive(Clone, Debug)]
pub struct MPPPartitionColumn {
    /// Col.
    pub col: Arc<Column>,
    /// CollateID.
    pub collate_id: i32,
}

impl MPPPartitionColumn {
    /// ResolveIndices resolve index for MPPPartitionColumn
    // go: MPPPartitionColumn.ResolveIndices L1
    pub fn resolve_indices(&self, schema: &Schema) -> Result<MPPPartitionColumn, tidb_base::Error> {
        let new_col = self.col.resolve_indices(schema)?;
        Ok(MPPPartitionColumn {
            col: Arc::new(new_col),
            collate_id: self.collate_id,
        })
    }

    /// Clone makes a copy of MPPPartitionColumn.
    // go: MPPPartitionColumn.Clone L1 [M-EXPR]
    pub fn clone_column(&self) -> MPPPartitionColumn {
        MPPPartitionColumn {
            col: Arc::clone(&self.col),
            collate_id: self.collate_id,
        }
    }

    /// `hashCode`.
    // go: MPPPartitionColumn.hashCode L1
    pub fn hash_code(&self) -> Vec<u8> {
        let mut hashcode = self.col.hash_code().to_vec();
        if self.collate_id < 0 {
            // collateId < 0 means new collation is not enabled
            encode_int(&mut hashcode, i64::from(self.collate_id));
        } else {
            encode_int(&mut hashcode, 1);
        }
        hashcode
    }

    /// Equal returns true if partitionCol == other
    // go: MPPPartitionColumn.Equal L1
    pub fn equal(&self, other: &MPPPartitionColumn) -> bool {
        if self.collate_id < 0 {
            // collateId only matters if new collation is enabled
            if self.collate_id != other.collate_id {
                return false;
            }
        }
        self.col.equal_col(&other.col)
    }

    /// MemoryUsage return the memory usage of MPPPartitionColumn
    // go: MPPPartitionColumn.MemoryUsage L1
    pub fn memory_usage(&self) -> i64 {
        SizeOfInt32 + self.col.memory_usage()
    }
}

/// ExplainColumnList generates explain information for a list of columns.
// go: ExplainColumnList L1
pub fn explain_column_list(ctx: &dyn EvalContext, cols: &[Arc<MPPPartitionColumn>]) -> Vec<u8> {
    let mut buffer = Vec::new();
    for (i, col) in cols.iter().enumerate() {
        buffer.extend_from_slice(b"[name: ");
        buffer.extend(col.col.explain_info(ctx));
        buffer.extend_from_slice(b", collate: ");
        if collate::new_collation_enabled() {
            buffer.extend(get_collate_name_by_id_for_partition(col.collate_id).into_bytes());
        } else {
            buffer.extend_from_slice(b"N/A");
        }
        buffer.extend_from_slice(b"]");
        if i + 1 < cols.len() {
            buffer.extend_from_slice(b", ");
        }
    }
    buffer
}

/// GetCollateIDByNameForPartition returns collate id by collation name
// go: GetCollateIDByNameForPartition L1 [M-INT]
pub fn get_collate_id_by_name_for_partition(coll: &str) -> i32 {
    let collate_id = collate::collation_name2_id(coll) as i32;
    collate::rewrite_new_collation_id_if_needed(collate_id)
}

/// GetCollateNameByIDForPartition returns collate id by collation name
// go: GetCollateNameByIDForPartition L1
pub fn get_collate_name_by_id_for_partition(collate_id: i32) -> String {
    let collate_id = collate::restore_collation_id_if_needed(collate_id);
    collate::collation_id2_name(collate_id)
}

/// cteProducerStatus indicates whether we can let the current CTE consumer/reader be executed on the MPP nodes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct CteProducerStatus(pub isize);

/// NoCTEOrAllProducerCanMPP.
pub const NoCTEOrAllProducerCanMPP: CteProducerStatus = CteProducerStatus(0);
/// SomeCTEFailedMpp.
pub const SomeCTEFailedMpp: CteProducerStatus = CteProducerStatus(1);
/// AllCTECanMpp.
pub const AllCTECanMpp: CteProducerStatus = CteProducerStatus(2);

/// PhysicalPropMatchResult describes the result of matching PhysicalProperty against an access path.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct PhysicalPropMatchResult(pub isize);

/// PropNotMatched means the access path cannot satisfy the required order.
pub const PropNotMatched: PhysicalPropMatchResult = PhysicalPropMatchResult(0);
/// PropMatched means the access path can satisfy the required property directly.
pub const PropMatched: PhysicalPropMatchResult = PhysicalPropMatchResult(1);
/// PropMatchedNeedMergeSort means the access path can satisfy the required property, but a merge sort between range
/// groups is needed.
/// Corresponding information will be recorded in AccessPath.GroupedRanges and AccessPath.GroupByColIdxs.
pub const PropMatchedNeedMergeSort: PhysicalPropMatchResult = PhysicalPropMatchResult(2);

impl PhysicalPropMatchResult {
    /// Matched returns true if the required order can be satisfied.
    // go: PhysicalPropMatchResult.Matched L1
    pub fn matched(self) -> bool {
        self == PropMatched || self == PropMatchedNeedMergeSort
    }
}

/// The anonymous `VectorProp` struct of `PhysicalProperty`.
#[derive(Clone, Debug, Default)]
pub struct VectorProp {
    /// The embedded `*expression.VectorHelper`.
    pub vector_helper: Option<Arc<VectorHelper>>,
    /// TopK.
    pub top_k: u32,
}

/// `hashcode` with Go's capacity of the slice (`MemoryUsage` adds `cap(p.hashcode)`).
/// Opaque: only `HashCode()` fills it.
#[derive(Clone, Debug)]
pub struct HashCode {
    bytes: Vec<u8>,
    cap: usize,
}

/// PhysicalProperty stands for the required physical property by parents.
/// It contains the orders and the task types.
#[derive(Clone, Debug, Default)]
pub struct PhysicalProperty {
    /// SortItems contains the required sort attributes.
    pub sort_items: Vec<SortItem>,

    /// TaskTp means the type of task that an operator requires.
    ///
    /// It needs to be specified because two different tasks can't be compared
    /// with cost directly. e.g. If a copTask takes less cost than a rootTask,
    /// we can't sure that we must choose the former one. Because the copTask
    /// must be finished and increase its cost in sometime, but we can't make
    /// sure the finishing time. So the best way to let the comparison fair is
    /// to add TaskType to required property.
    pub task_tp: TaskType,

    /// ExpectedCnt means this operator may be closed after fetching ExpectedCnt
    /// records.
    pub expected_cnt: f64,

    /// hashcode stores the hash code of a PhysicalProperty, will be lazily
    /// calculated when function "HashCode()" being called. Public only so that the property
    /// can be built with a struct literal (`..Default::default()`), as Go does.
    pub hashcode: OnceLock<HashCode>,

    /// indicates that whether we are allowed to add an enforcer.
    pub can_add_enforcer: bool,

    /// If the partition type is hash, the data should be reshuffled by partition cols.
    pub mpp_partition_cols: Vec<Arc<MPPPartitionColumn>>,

    /// which types the exchange sender belongs to, only take effects when it's a mpp task.
    pub mpp_partition_tp: MPPPartitionType,

    /// SortItemsForPartition means these sort only need to sort the data of one partition, instead of global.
    /// It is added only if it is used to sort the sharded data of the window function.
    /// Non-MPP tasks do not care about it.
    pub sort_items_for_partition: Vec<SortItem>,

    /// RejectSort means rejecting the sort property from its children, but it only works for MPP tasks.
    /// Non-MPP tasks do not care about it.
    pub reject_sort: bool,

    /// CTEProducerStatus.
    pub cte_producer_status: CteProducerStatus,

    /// VectorProp.
    pub vector_prop: VectorProp,

    /// PartialOrderInfo is used for TopN's partial order optimization.
    /// When this field is not nil, it indicates that prefix index can be used
    /// to provide partial order for TopN.
    pub partial_order_info: Option<Arc<PartialOrderInfo>>,

    /// AdvisorySortItems contains sort items that are preferred but not required.
    /// When SortItems is empty and AdvisorySortItems is not, DataSource can try to
    /// generate paths that satisfy these sort items, enabling Limit pushdown
    /// to partial paths of IndexMerge.
    /// Currently only set when TopN is directly above a DataSource.
    pub advisory_sort_items: Vec<SortItem>,
}

/// PartialOrderInfo records information needed for partial order optimization.
/// When PhysicalProperty.PartialOrderInfo is not nil, it indicates that
/// prefix index can be used to provide partial order.
#[derive(Clone, Debug, Default)]
pub struct PartialOrderInfo {
    /// SortItems are the ORDER BY columns from TopN.
    pub sort_items: Vec<SortItem>,
}

impl PartialOrderInfo {
    /// AllSameOrder checks if all the items have same order.
    // go: PartialOrderInfo.AllSameOrder L1
    pub fn all_same_order(&self) -> (bool, bool) {
        if self.sort_items.is_empty() {
            return (true, false);
        }
        for i in 1..self.sort_items.len() {
            if self.sort_items[i].desc != self.sort_items[i - 1].desc {
                return (false, false);
            }
        }
        (true, self.sort_items[0].desc)
    }
}

/// PartialOrderMatchResult records the result of matching partial order property with an access path.
#[derive(Clone, Debug, Default)]
pub struct PartialOrderMatchResult {
    /// Matched indicates whether this path can provide partial order.
    pub matched: bool,
    /// PrefixCol is the last and only one prefix column in the matched index.
    pub prefix_col: Option<Arc<Column>>,
    /// PrefixLen is the prefix length in bytes for prefix index.
    pub prefix_len: isize,
}

/// NewPhysicalProperty builds property from columns.
// go: NewPhysicalProperty L1
pub fn new_physical_property(
    task_tp: TaskType,
    cols: &[Arc<Column>],
    desc: bool,
    expect_cnt: f64,
    enforced: bool,
) -> PhysicalProperty {
    PhysicalProperty {
        sort_items: sort_items_from_cols(cols, desc),
        task_tp,
        expected_cnt: expect_cnt,
        can_add_enforcer: enforced,
        ..Default::default()
    }
}

/// SortItemsFromCols builds property items from columns.
// go: SortItemsFromCols L1
pub fn sort_items_from_cols(cols: &[Arc<Column>], desc: bool) -> Vec<SortItem> {
    let mut items = Vec::with_capacity(cols.len());
    for col in cols {
        items.push(SortItem {
            col: Arc::clone(col),
            desc,
        });
    }
    items
}

/// Appends `src` to the hash code buffer, growing Go's capacity as `append` does.
// glue: Go append on p.hashcode (capacity bookkeeping for MemoryUsage)
fn hc_append(h: &mut HashCode, src: &[u8]) {
    let new_len = h.bytes.len() + src.len();
    if new_len > h.cap {
        h.cap = growslice_cap(new_len, h.cap, 1);
    }
    h.bytes.extend_from_slice(src);
}

/// `codec.EncodeInt` on the hash code buffer.
// glue: Go append on p.hashcode
fn hc_int(h: &mut HashCode, v: i64) {
    let mut b = Vec::with_capacity(8);
    encode_int(&mut b, v);
    hc_append(h, &b);
}

impl PhysicalProperty {
    /// IsSubsetOf check if the keys can match the needs of partition.
    ///
    /// Go's nil result is `None`.
    // go: PhysicalProperty.IsSubsetOf L1
    pub fn is_subset_of(&self, keys: &[Arc<MPPPartitionColumn>]) -> Option<Vec<isize>> {
        if self.mpp_partition_cols.len() > keys.len() {
            return None;
        }
        let mut matches = Vec::with_capacity(keys.len());
        for part_col in &self.mpp_partition_cols {
            let mut found = false;
            for (i, key) in keys.iter().enumerate() {
                if part_col.equal(key) {
                    found = true;
                    matches.push(i as isize);
                    break;
                }
            }
            if !found {
                return None;
            }
        }
        Some(matches)
    }

    /// AllColsFromSchema checks whether all the columns needed by this physical
    /// property can be found in the given schema.
    // go: PhysicalProperty.AllColsFromSchema L1
    pub fn all_cols_from_schema(&self, schema: &Schema) -> bool {
        for col in &self.sort_items {
            if schema.column_index(&col.col) == -1 {
                return false;
            }
        }
        true
    }

    /// IsFlashProp return true if this physical property is only allowed to generate flash related task
    // go: PhysicalProperty.IsFlashProp L1
    pub fn is_flash_prop(&self) -> bool {
        self.task_tp == MppTaskType
    }

    /// GetAllPossibleChildTaskTypes enumrates the possible types of tasks for children.
    // go: PhysicalProperty.GetAllPossibleChildTaskTypes L1
    pub fn get_all_possible_child_task_types(&self) -> Vec<TaskType> {
        if self.task_tp == RootTaskType {
            return wholeTaskTypes.to_vec();
        }
        // TODO: For CopSingleReadTaskType and CopDoubleReadTaskType, this function should never be called
        vec![self.task_tp]
    }

    /// IsPrefix checks whether the order property is the prefix of another.
    // go: PhysicalProperty.IsPrefix L1
    pub fn is_prefix(&self, prop: &PhysicalProperty) -> bool {
        if self.sort_items.len() > prop.sort_items.len() {
            return false;
        }
        for i in 0..self.sort_items.len() {
            if !self.sort_items[i].col.equal_col(&prop.sort_items[i].col)
                || self.sort_items[i].desc != prop.sort_items[i].desc
            {
                return false;
            }
        }
        true
    }

    /// IsSortItemAllForPartition check whether SortItems is same as SortItemsForPartition
    // go: PhysicalProperty.IsSortItemAllForPartition L1
    pub fn is_sort_item_all_for_partition(&self) -> bool {
        if self.sort_items_for_partition.len() != self.sort_items.len() {
            return false;
        }
        for i in 0..self.sort_items_for_partition.len() {
            if !self.sort_items_for_partition[i]
                .col
                .equal_col(&self.sort_items[i].col)
                || self.sort_items_for_partition[i].desc != self.sort_items[i].desc
            {
                return false;
            }
        }
        true
    }

    /// IsSortItemEmpty checks whether the order property is empty.
    // go: PhysicalProperty.IsSortItemEmpty L1
    pub fn is_sort_item_empty(&self) -> bool {
        self.sort_items.is_empty()
    }

    /// NeedKeepOrder returns whether the property requires maintaining order.
    // go: PhysicalProperty.NeedKeepOrder L1
    pub fn need_keep_order(&self) -> bool {
        !self.is_sort_item_empty() || self.partial_order_info.is_some()
    }

    /// GetSortDescForKeepOrder returns the sort direction for keep-order scans.
    // go: PhysicalProperty.GetSortDescForKeepOrder L1
    pub fn get_sort_desc_for_keep_order(&self) -> bool {
        if let Some(poi) = &self.partial_order_info
            && !poi.sort_items.is_empty()
        {
            let (_, desc) = poi.all_same_order();
            return desc;
        }
        let (_, desc) = self.all_same_order();
        desc
    }

    /// GetSortItemsForKeepOrder returns the sort items used for keep-order scans.
    // go: PhysicalProperty.GetSortItemsForKeepOrder L1
    pub fn get_sort_items_for_keep_order(&self) -> Vec<SortItem> {
        if let Some(poi) = &self.partial_order_info
            && !poi.sort_items.is_empty()
        {
            let mut items = Vec::with_capacity(poi.sort_items.len());
            for si in &poi.sort_items {
                items.push(si.clone());
            }
            return items;
        }
        self.sort_items.clone()
    }

    /// HashCode calculates hash code for a PhysicalProperty object.
    // go: PhysicalProperty.HashCode L1
    pub fn hash_code(&self) -> &[u8] {
        &self.hashcode.get_or_init(|| self.compute_hash_code()).bytes
    }

    /// The body of `HashCode` after the cache check.
    // go-part: PhysicalProperty.HashCode
    fn compute_hash_code(&self) -> HashCode {
        let mut hashcode_size = 8
            + 8
            + 8
            + (16 + 8) * self.sort_items.len()
            + 8
            + (16 + 8) * self.advisory_sort_items.len()
            + 8;
        if let Some(poi) = &self.partial_order_info {
            hashcode_size += (16 + 8) * poi.sort_items.len();
        } else {
            hashcode_size += 8;
        }
        let mut h = HashCode {
            bytes: Vec::with_capacity(hashcode_size),
            cap: hashcode_size,
        };
        if self.can_add_enforcer {
            hc_int(&mut h, 1);
        } else {
            hc_int(&mut h, 0);
        }
        hc_int(&mut h, self.task_tp.0 as i64);
        let mut f = Vec::with_capacity(8);
        encode_float(&mut f, self.expected_cnt);
        hc_append(&mut h, &f);
        for item in &self.sort_items {
            hc_append(&mut h, item.col.hash_code());
            if item.desc {
                hc_int(&mut h, 1);
            } else {
                hc_int(&mut h, 0);
            }
        }
        if self.task_tp == MppTaskType {
            hc_int(&mut h, self.mpp_partition_tp.0 as i64);
            for col in &self.mpp_partition_cols {
                hc_append(&mut h, &col.hash_code());
            }
            if let Some(vh) = &self.vector_prop.vector_helper {
                // We only accpect the vector information from the TopN which is directly above the DataSource.
                // So it's safe to not hash the vector constant.
                hc_append(&mut h, vh.column.hash_code());
                hc_int(&mut h, i64::from(vh.fn_pb_code));
            }
        }
        // `append(p.hashcode, codec.EncodeInt(nil, ...)...)`: the same bytes.
        hc_int(&mut h, self.cte_producer_status.0 as i64);
        if let Some(poi) = &self.partial_order_info {
            hc_int(&mut h, 1);
            for item in &poi.sort_items {
                hc_append(&mut h, item.col.hash_code());
                if item.desc {
                    hc_int(&mut h, 1);
                } else {
                    hc_int(&mut h, 0);
                }
            }
        } else {
            hc_int(&mut h, 0);
        }
        // Encode AdvisorySortItems into the physical property's hashcode.
        for item in &self.advisory_sort_items {
            hc_append(&mut h, item.col.hash_code());
            if item.desc {
                hc_int(&mut h, 1);
            } else {
                hc_int(&mut h, 0);
            }
        }
        h
    }

    /// String implements fmt.Stringer interface. Just for test.
    ///
    /// Go formats `p.SortItems` (a `[]SortItem`, whose `String` has a pointer receiver) by
    /// reflection: each item prints as `{<column pointer> <desc>}`, the pointer being the
    /// address of the `*Column`. The address of the column here stands for it.
    // go: PhysicalProperty.String L1 [M-NAME]
    pub fn to_string(&self) -> Vec<u8> {
        let items: Vec<String> = self
            .sort_items
            .iter()
            .map(|it| format!("{{{:p} {}}}", Arc::as_ptr(&it.col), it.desc))
            .collect();
        let cnt =
            tidb_base::gofmt::sprintf("%v", &[tidb_base::gofmt::Arg::Float(self.expected_cnt, 64)]);
        let mut out = format!(
            "Prop{{cols: [{}], TaskTp: {}, expectedCount: ",
            items.join(" "),
            self.task_tp.to_string()
        )
        .into_bytes();
        out.extend(cnt);
        out.push(b'}');
        out
    }

    /// CloneEssentialFields returns a copy of PhysicalProperty. We only copy the essential fields that really indicate the
    /// property, specifically, `CanAddEnforcer` should not be included.
    // go: PhysicalProperty.CloneEssentialFields L1
    pub fn clone_essential_fields(&self) -> PhysicalProperty {
        PhysicalProperty {
            sort_items: self.sort_items.clone(),
            sort_items_for_partition: self.sort_items_for_partition.clone(),
            task_tp: self.task_tp,
            expected_cnt: self.expected_cnt,
            mpp_partition_tp: self.mpp_partition_tp,
            mpp_partition_cols: self.mpp_partition_cols.clone(),
            reject_sort: self.reject_sort,
            cte_producer_status: self.cte_producer_status,
            partial_order_info: self.partial_order_info.clone(),
            advisory_sort_items: self.advisory_sort_items.clone(),
            ..Default::default()
        }
    }

    /// AllSameOrder checks if all the items have same order.
    // go: PhysicalProperty.AllSameOrder L1
    pub fn all_same_order(&self) -> (bool, bool) {
        if self.sort_items.is_empty() {
            return (true, false);
        }
        for i in 1..self.sort_items.len() {
            if self.sort_items[i].desc != self.sort_items[i - 1].desc {
                return (false, false);
            }
        }
        (true, self.sort_items[0].desc)
    }

    /// MemoryUsage return the memory usage of PhysicalProperty
    // go: PhysicalProperty.MemoryUsage L2 (unsafe.Sizeof(PhysicalProperty{}) is Go's size on 64-bit targets)
    pub fn memory_usage(&self) -> i64 {
        let hashcode_cap = self.hashcode.get().map_or(0, |h| h.cap) as i64;
        let mut sum = emptyPhysicalPropertySize + hashcode_cap;
        for sort_item in &self.sort_items {
            sum += sort_item.memory_usage();
        }
        for sort_item in &self.sort_items_for_partition {
            sum += sort_item.memory_usage();
        }
        for sort_item in &self.advisory_sort_items {
            sum += sort_item.memory_usage();
        }
        for mpp_col in &self.mpp_partition_cols {
            sum += mpp_col.memory_usage();
        }
        sum
    }
}

/// `unsafe.Sizeof(PhysicalProperty{})` of Go v8.5.8 on amd64 / arm64.
const emptyPhysicalPropertySize: i64 = 192;
