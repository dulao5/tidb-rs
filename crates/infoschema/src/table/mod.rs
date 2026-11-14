//! go-file: pkg/table/table.go
//!
//! Package `table` (block D06.4 part 1, specs/D06.4-table): the `Table` / `Index` interfaces,
//! the column metadata and default values ([`column`]), the index interface ([`index`]).
//! The implementation is [`crate::tables`] (`pkg/table/tables`).
//!
//! tidb-rs is read-only: the write half of the interfaces (`AddRecord`, `UpdateRecord`,
//! `RemoveRecord`, `Index.Create` / `Delete`, the mutate options, auto-ID allocation, check
//! constraints) is not ported (permanent exemption "write path", D06.4 spec §1.2).
//! Partitioned, cached and temporary tables are part 2: [`PartitionedTable`] and
//! [`CachedTable`] are markers until then, so that callers see the table kind and stop at a
//! boundary stub instead of reading such a table as a plain one.

// Error variables keep their Go names (mapping rule M-GONAME).
#![allow(non_upper_case_globals)]

pub mod column;
pub mod index;

use std::sync::{Arc, LazyLock};

use tidb_base::Error;
use tidb_base::dbterror::{ClassDDL, ClassExecutor, ClassTable};
use tidb_base::errno;
use tidb_kv::key::Key;
use tidb_model::TableInfo;

use crate::autoid::Allocators;
use crate::sieve::SieveSize;

pub use column::{Column, ColumnRef};
pub use index::{Index, IndexRef};

/// Type is used to distinguish between different tables that store data in different ways.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Type(pub i16);

/// NormalTable stores data in tikv, mocktikv and so on.
pub const NormalTable: Type = Type(0);
/// VirtualTable stores no data, just extract data from the memory struct.
pub const VirtualTable: Type = Type(1);
/// ClusterTable contains the `VirtualTable` in the all cluster tidb nodes.
pub const ClusterTable: Type = Type(2);

impl Type {
    /// IsNormalTable checks whether the table is a normal table type.
    // go: Type.IsNormalTable L1
    pub fn is_normal_table(self) -> bool {
        self == NormalTable
    }

    /// IsVirtualTable checks whether the table is a virtual table type.
    // go: Type.IsVirtualTable L1
    pub fn is_virtual_table(self) -> bool {
        self == VirtualTable
    }

    /// IsClusterTable checks whether the table is a cluster table type.
    // go: Type.IsClusterTable L1
    pub fn is_cluster_table(self) -> bool {
        self == ClusterTable
    }
}

macro_rules! table_err {
    ($(#[$m:meta])* $name:ident, $class:ident, $code:ident) => {
        $(#[$m])*
        pub static $name: LazyLock<Error> = LazyLock::new(|| $class.new_std(errno::$code));
    };
}

table_err!(
    /// ErrColumnCantNull is used for inserting null to a not null column.
    ErrColumnCantNull, ClassTable, ErrBadNull
);
table_err!(
    /// ErrUnknownColumn is returned when accessing an unknown column.
    ErrUnknownColumn, ClassTable, ErrBadField
);
table_err!(errDuplicateColumn, ClassTable, ErrFieldSpecifiedTwice);
table_err!(
    /// ErrWarnNullToNotnull is like ErrColumnCantNull but it's used in LOAD DATA
    ErrWarnNullToNotnull, ClassExecutor, ErrWarnNullToNotnull
);
table_err!(errGetDefaultFailed, ClassTable, ErrFieldGetDefaultFailed);
table_err!(
    /// ErrNoDefaultValue is used when insert a row, the column value is not given, and the
    /// column has not null flag and it doesn't have a default value.
    ErrNoDefaultValue, ClassTable, ErrNoDefaultForField
);
table_err!(
    /// ErrIndexOutBound returns for index column offset out of bound.
    ErrIndexOutBound, ClassTable, ErrIndexOutBound
);
table_err!(
    /// ErrUnsupportedOp returns for unsupported operation.
    ErrUnsupportedOp, ClassTable, ErrUnsupportedOp
);
table_err!(
    /// ErrRowNotFound returns for row not found.
    ErrRowNotFound, ClassTable, ErrRowNotFound
);
table_err!(
    /// ErrTableStateCantNone returns for table none state.
    ErrTableStateCantNone, ClassTable, ErrTableStateCantNone
);
table_err!(
    /// ErrColumnStateCantNone returns for column none state.
    ErrColumnStateCantNone, ClassTable, ErrColumnStateCantNone
);
table_err!(
    /// ErrColumnStateNonPublic returns for column non-public state.
    ErrColumnStateNonPublic, ClassTable, ErrColumnStateNonPublic
);
table_err!(
    /// ErrIndexStateCantNone returns for index none state.
    ErrIndexStateCantNone, ClassTable, ErrIndexStateCantNone
);
table_err!(
    /// ErrInvalidRecordKey returns for invalid record key.
    ErrInvalidRecordKey, ClassTable, ErrInvalidRecordKey
);
table_err!(
    /// ErrTruncatedWrongValueForField returns for truncate wrong value for field.
    ErrTruncatedWrongValueForField, ClassTable, ErrTruncatedWrongValueForField
);
table_err!(
    /// ErrUnknownPartition returns unknown partition error.
    ErrUnknownPartition, ClassTable, ErrUnknownPartition
);
table_err!(
    /// ErrNoPartitionForGivenValue returns table has no partition for value.
    ErrNoPartitionForGivenValue, ClassTable, ErrNoPartitionForGivenValue
);
table_err!(
    /// ErrLockOrActiveTransaction returns when execute unsupported statement in a lock session or an active transaction.
    ErrLockOrActiveTransaction, ClassTable, ErrLockOrActiveTransaction
);
table_err!(
    /// ErrSequenceHasRunOut returns when sequence has run out.
    ErrSequenceHasRunOut, ClassTable, ErrSequenceRunOut
);
table_err!(
    /// ErrRowDoesNotMatchGivenPartitionSet returns when the destination partition conflict with the partition selection.
    ErrRowDoesNotMatchGivenPartitionSet, ClassTable, ErrRowDoesNotMatchGivenPartitionSet
);
table_err!(
    /// ErrTempTableFull returns a table is full error, it's used by temporary table now.
    ErrTempTableFull, ClassTable, ErrRecordFileFull
);
table_err!(
    /// ErrOptOnCacheTable returns when exec unsupported opt at cache mode
    ErrOptOnCacheTable, ClassDDL, ErrOptOnCacheTable
);
table_err!(
    /// ErrCheckConstraintViolated return when check constraint is violated.
    ErrCheckConstraintViolated, ClassTable, ErrCheckConstraintViolated
);

/// Table is used to retrieve and modify rows in table.
///
/// The read half of Go's interface (`columnAPI`, `Indices`, `RecordPrefix`, `IndexPrefix`,
/// `Allocators`, `Meta`, `Type`, `GetPartitionedTable`). `WritableConstraint` and the
/// record mutations are the write path; `Allocators` takes no `AllocatorContext` (Go uses it
/// only for the alternative allocators of temporary tables, part 2).
pub trait Table: Send + Sync {
    /// Cols returns the columns of the table which is used in select, including hidden columns.
    // glue: interface method Table.Cols (columnAPI)
    fn cols(&self) -> &[ColumnRef];
    /// VisibleCols returns the columns of the table which is used in select, excluding hidden columns.
    // glue: interface method Table.VisibleCols (columnAPI)
    fn visible_cols(&self) -> &[ColumnRef];
    /// HiddenCols returns the hidden columns of the table.
    // glue: interface method Table.HiddenCols (columnAPI)
    fn hidden_cols(&self) -> &[ColumnRef];
    /// WritableCols returns columns of the table in writable states.
    // glue: interface method Table.WritableCols (columnAPI)
    fn writable_cols(&self) -> &[ColumnRef];
    /// DeletableCols returns columns of the table in deletable states.
    // glue: interface method Table.DeletableCols (columnAPI)
    fn deletable_cols(&self) -> &[ColumnRef];
    /// FullHiddenColsAndVisibleCols returns hidden columns in all states and unhidden
    /// columns in public states.
    // glue: interface method Table.FullHiddenColsAndVisibleCols (columnAPI)
    fn full_hidden_cols_and_visible_cols(&self) -> &[ColumnRef];
    /// Indices returns the indices of the table.
    // glue: interface method Table.Indices
    fn indices(&self) -> &[IndexRef];
    /// DeletableIndices returns delete-only indices of the table.
    // glue: interface method Table.DeletableIndices
    fn deletable_indices(&self) -> &[IndexRef];
    /// RecordPrefix returns the record key prefix.
    // glue: interface method Table.RecordPrefix
    fn record_prefix(&self) -> &Key;
    /// IndexPrefix returns the index key prefix.
    // glue: interface method Table.IndexPrefix
    fn index_prefix(&self) -> &Key;
    /// Allocators returns all allocators.
    // glue: interface method Table.Allocators
    fn allocators(&self) -> Allocators;
    /// Meta returns TableInfo.
    // glue: interface method Table.Meta
    fn meta(&self) -> &Arc<TableInfo>;
    /// Type returns the type of table
    // glue: interface method Table.Type
    fn type_(&self) -> Type;
    /// GetPartitionedTable returns nil if not partitioned
    // glue: interface method Table.GetPartitionedTable
    fn get_partitioned_table(&self) -> Option<&dyn PartitionedTable>;
    /// The table as a `PhysicalTable` (Go: the type assertion `t.(table.PhysicalTable)`).
    // glue: type assertion (mapping rule M-IFACE)
    fn as_physical_table(&self) -> Option<&dyn PhysicalTable>;
    /// The table as a `CachedTable` (Go: the type assertion `t.(table.CachedTable)`).
    // glue: type assertion (mapping rule M-IFACE)
    fn as_cached_table(&self) -> Option<&dyn CachedTable>;
    /// The heap size of the table (exception A3: the size the schema cache charges for it).
    // glue: A3 (Rust memory layout of the cached tables)
    fn mem_size(&self) -> u64;
    /// The concrete table (Go: a type assertion to the struct, e.g. `t.(*tables.TableCommon)`).
    // glue: type assertion (mapping rule M-IFACE)
    fn as_any(&self) -> &dyn std::any::Any;
}

/// A shared `table.Table` interface value.
pub type TableRef = Arc<dyn Table>;

impl SieveSize for TableRef {
    // glue: A3
    fn heap_size(&self) -> u64 {
        self.mem_size()
    }
}

/// PhysicalTable is an abstraction for two kinds of table representation: partition or
/// non-partitioned table. PhysicalID is a ID that can be used to construct a key ranges, all
/// the data in the key range belongs to the corresponding PhysicalTable. For a
/// non-partitioned table, its PhysicalID equals to its TableID; For a partition of a
/// partitioned table, its PhysicalID is the partition's ID.
pub trait PhysicalTable: Table {
    // glue: interface method PhysicalTable.GetPhysicalID
    fn get_physical_id(&self) -> i64;
}

/// PartitionedTable is a Table, and it has a GetPartition() method. GetPartition() gets the
/// partition from a partition table by a physical table ID.
///
/// Part 1 marker: Go's methods (`GetPartition`, `GetPartitionByRow`, `GetAllPartitionIDs`,
/// ...) come with `partition.go` in part 2 (blocked-by:D06.4); until then a caller that
/// finds a partitioned table must stop at its own boundary stub.
pub trait PartitionedTable: Table {}

/// CachedTable is a Table, and it has a UpdateLockForRead() method. UpdateLockForRead()
/// according to the reasons for not meeting the read conditions, update the lock information,
/// And at the same time reload data from the original table.
///
/// Part 1 marker: Go's methods (`Init`, `TryReadFromCache`, `UpdateLockForRead`,
/// `WriteLockAndKeepAlive`) come with `cache.go` in part 2 (blocked-by:D06.4).
pub trait CachedTable: Table {}

/// TableFromMeta builds a table.Table from *model.TableInfo. Currently, it is assigned to
/// tables.TableFromMeta in tidb package's init function.
// go-part: pkg/table/tables/tables.go init (the assignment `table.TableFromMeta = TableFromMeta`)
pub fn table_from_meta(allocs: Allocators, tbl_info: Arc<TableInfo>) -> Result<TableRef, Error> {
    crate::tables::table_from_meta(allocs, tbl_info)
}

/// MockTableFromMeta only serves for test.
// go-part: pkg/table/tables/tables.go init (the assignment `table.MockTableFromMeta = MockTableFromMeta`)
pub fn mock_table_from_meta(tbl_info: Arc<TableInfo>) -> Option<TableRef> {
    crate::tables::mock_table_from_meta(tbl_info)
}

/// The estimated heap size of a table info (exception A3, D06.3 spec §5.3): the struct, its
/// columns, indices, partitions and foreign keys, and their names. An estimate: what the
/// schema cache charges against `tidb_schema_cache_size`.
// glue: A3 (Rust size estimate of model.TableInfo)
pub fn table_info_mem_size(t: &TableInfo) -> u64 {
    use std::mem::size_of;
    let mut n = size_of::<TableInfo>() + t.name.o.len() + t.name.l.len() + t.comment.len();
    for c in t.columns.iter().flatten() {
        n += size_of::<tidb_model::ColumnInfo>()
            + c.name.o.len()
            + c.name.l.len()
            + c.comment.len()
            + c.generated_expr_string.len();
    }
    for i in t.indices.iter().flatten() {
        n += size_of::<tidb_model::IndexInfo>() + i.name.o.len() + i.name.l.len();
        n += i.columns.iter().flatten().count() * size_of::<tidb_model::IndexColumn>();
    }
    for f in t.foreign_keys.iter().flatten() {
        n += size_of::<tidb_model::FKInfo>() + f.name.o.len() + f.name.l.len();
    }
    if let Some(p) = &t.partition {
        n += size_of::<tidb_model::PartitionInfo>();
        n += p.definitions.iter().flatten().count() * size_of::<tidb_model::PartitionDefinition>();
    }
    n as u64
}
