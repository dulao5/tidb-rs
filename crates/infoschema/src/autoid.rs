//! go-file: pkg/meta/autoid/autoid.go
//!
//! `pkg/meta/autoid` is block D06.2 part 2 (specs/D06.2-meta/plan.md). D06.3 needs a few of
//! its names; until part 2 lands this module holds:
//! - the constants and [`is_mem_schema_id`], which reference nothing else (constitution
//!   4.5: a function without out-edges may be ported early, L1, and counts for its block);
//! - stand-ins for the allocator types the InfoSchema builder passes around
//!   ([`Requirement`], [`Allocator`], [`Allocators`], [`new_allocators_from_tbl_info`]).
//!   tidb-rs is read-only: allocators only matter for `SHOW TABLE NEXT_ROW_ID` and
//!   `information_schema.tables.AUTO_INCREMENT`, which come with part 2.

#![allow(non_upper_case_globals)]

use std::sync::Arc;

use tidb_base::Error;
use tidb_kv::kv::Storage;
use tidb_model::TableInfo;

/// SystemSchemaIDFlag is the system schema/table id flag, uses the highest bit position as
/// system schema ID flag, it's exports for test.
pub const SystemSchemaIDFlag: i64 = 1 << 62;
/// InformationSchemaDBID is the information_schema schema id, it's exports for test.
pub const InformationSchemaDBID: i64 = SystemSchemaIDFlag | 1;
/// PerformanceSchemaDBID is the performance_schema schema id, it's exports for test.
pub const PerformanceSchemaDBID: i64 = SystemSchemaIDFlag | 10000;
/// MetricSchemaDBID is the metrics_schema schema id, it's exported for test.
pub const MetricSchemaDBID: i64 = SystemSchemaIDFlag | 20000;

/// IsMemSchemaID checks whether the schema ID belongs to a memory schema.
// go: IsMemSchemaID L1
pub fn is_mem_schema_id(schema_id: i64) -> bool {
    schema_id & SystemSchemaIDFlag != 0
}

/// AllocatorType is the type of allocator for generating auto-id. Different type of
/// allocators use different key-value pairs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AllocatorType(pub u8);

/// RowIDAllocType indicates the allocator is used to allocate row id.
pub const RowIDAllocType: AllocatorType = AllocatorType(0);
/// AutoIncrementType indicates the allocator is used to allocate auto increment value.
pub const AutoIncrementType: AllocatorType = AllocatorType(1);
/// AutoRandomType indicates the allocator is used to allocated auto-shard id.
pub const AutoRandomType: AllocatorType = AllocatorType(2);
/// SequenceType indicates the allocator is used to allocated sequence value.
pub const SequenceType: AllocatorType = AllocatorType(3);

/// Requirement is the parameter required by NewAllocator.
// glue: stand-in: D06.2 part 2, autoid.Requirement (AutoIDClient is the autoid service, X)
pub trait Requirement: Send + Sync {
    // glue: stand-in: D06.2 part 2, interface method Requirement.Store
    fn store(&self) -> Arc<dyn Storage>;
}

/// Allocator is an auto increment id generator. Only the methods the InfoSchema builder
/// calls.
// glue: stand-in: D06.2 part 2, autoid.Allocator
pub trait Allocator: Send + Sync {
    // glue: stand-in: D06.2 part 2, interface method Allocator.GetType
    fn get_type(&self) -> AllocatorType;
    // glue: stand-in: D06.2 part 2, interface method Allocator.Transfer
    fn transfer(&self, db_id: i64, table_id: i64) -> Result<(), Error>;
}

/// Allocators represents a set of `Allocator`s.
// glue: stand-in: D06.2 part 2, autoid.Allocators
#[derive(Clone, Default)]
pub struct Allocators {
    pub sep_auto_inc: bool,
    pub allocs: Vec<Arc<dyn Allocator>>,
}

impl Allocators {
    /// Append add an allocator to the allocators.
    // glue: stand-in: D06.2 part 2, Allocators.Append
    pub fn append(&self, a: Arc<dyn Allocator>) -> Allocators {
        let mut allocs = self.allocs.clone();
        allocs.push(a);
        Allocators {
            sep_auto_inc: self.sep_auto_inc,
            allocs,
        }
    }

    /// Get returns the Allocator according to the AllocatorType.
    // glue: stand-in: D06.2 part 2, Allocators.Get
    pub fn get(&self, alloc_type: AllocatorType) -> Option<&Arc<dyn Allocator>> {
        let alloc_type = if !self.sep_auto_inc && alloc_type == AutoIncrementType {
            RowIDAllocType
        } else {
            alloc_type
        };
        self.allocs.iter().find(|a| a.get_type() == alloc_type)
    }

    /// Filter filters all the allocators that match pred.
    // glue: stand-in: D06.2 part 2, Allocators.Filter
    pub fn filter(&self, pred: impl Fn(&dyn Allocator) -> bool) -> Allocators {
        Allocators {
            sep_auto_inc: self.sep_auto_inc,
            allocs: self
                .allocs
                .iter()
                .filter(|a| pred(a.as_ref()))
                .cloned()
                .collect(),
        }
    }
}

/// NewAllocatorsFromTblInfo creates an array of allocators of different types with the
/// information of model.TableInfo. The stand-in creates none: no allocator is used on the
/// read path before part 2.
// glue: stub, blocked-by:D06.2 (part 2, NewAllocatorsFromTblInfo)
pub fn new_allocators_from_tbl_info(
    _r: Option<&Arc<dyn Requirement>>,
    _db_id: i64,
    tbl_info: &TableInfo,
) -> Allocators {
    Allocators {
        sep_auto_inc: tbl_info.sep_auto_inc(),
        allocs: Vec::new(),
    }
}

/// NewAllocator returns a new auto increment id generator on the store. The stand-in
/// returns `None` (no allocator before part 2); the builder appends nothing then.
// glue: stub, blocked-by:D06.2 (part 2, NewAllocator)
pub fn new_allocator(
    _r: Option<&Arc<dyn Requirement>>,
    _db_id: i64,
    _tbl_id: i64,
    _is_unsigned: bool,
    _alloc_type: AllocatorType,
) -> Option<Arc<dyn Allocator>> {
    None
}
