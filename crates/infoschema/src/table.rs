//! Stand-in for `pkg/table` and `pkg/table/tables` (block D06.4, not started): the
//! InfoSchema stores and returns `table.Table` values. Until D06.4 a table is its
//! `*model.TableInfo` plus its allocators ([`MetaTable`]), which is all the InfoSchema
//! itself reads (`Meta()`, `Allocators()`).

use std::sync::Arc;

use tidb_base::Error;
use tidb_model::TableInfo;

use crate::autoid::Allocators;
use crate::sieve::SieveSize;

/// Table is used to retrieve and modify rows in table.
// glue: stand-in: D06.4, table.Table (the methods the InfoSchema calls)
pub trait Table: Send + Sync {
    // glue: stand-in: D06.4, interface method Table.Meta
    fn meta(&self) -> &Arc<TableInfo>;
    // glue: stand-in: D06.4, interface method Table.Allocators
    fn allocators(&self) -> Allocators;
    /// The heap size of the table (exception A3: the size the schema cache charges for it).
    // glue: A3 (Rust memory layout of the cached tables)
    fn mem_size(&self) -> u64;
}

/// A shared `table.Table` interface value.
pub type TableRef = Arc<dyn Table>;

impl SieveSize for TableRef {
    // glue: A3
    fn heap_size(&self) -> u64 {
        self.mem_size()
    }
}

/// The stand-in table: the table info and its allocators.
// glue: stand-in: D06.4, tables.TableCommon
pub struct MetaTable {
    meta: Arc<TableInfo>,
    allocs: Allocators,
    mem_size: u64,
}

impl Table for MetaTable {
    // glue: stand-in: D06.4, TableCommon.Meta
    fn meta(&self) -> &Arc<TableInfo> {
        &self.meta
    }

    // glue: stand-in: D06.4, TableCommon.Allocators
    fn allocators(&self) -> Allocators {
        self.allocs.clone()
    }

    // glue: A3
    fn mem_size(&self) -> u64 {
        self.mem_size
    }
}

/// TableFromMeta creates a Table instance from model.TableInfo. The stand-in keeps the
/// table info; the state checks, columns, indices, generated columns and the cached /
/// partitioned / temporary table kinds come with D06.4.
// glue: stub, blocked-by:D06.4 (tables.TableFromMeta)
pub fn table_from_meta(allocs: Allocators, tbl_info: Arc<TableInfo>) -> Result<TableRef, Error> {
    let mem_size = table_info_mem_size(&tbl_info);
    Ok(Arc::new(MetaTable {
        meta: tbl_info,
        allocs,
        mem_size,
    }))
}

/// The estimated heap size of a table info (exception A3, spec §5.3): the struct, its
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
