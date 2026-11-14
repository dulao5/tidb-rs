//! Memory of the long-lived schema structures with shared columns and indices (spec N13,
//! issue #229): 10,000 tables × 30 columns × 3 indices.
//!
//! jemalloc (the server's allocator; this binary has one test, so nothing else allocates
//! concurrently) gives the live heap bytes of each stage:
//!
//! 1. the decoded `TableInfo`s (the model);
//! 2. the table objects (`TableFromMeta`) of all tables, which share the model's
//!    `ColumnInfo` / `IndexInfo` (before #229 each table object copied them);
//! 3. what that copy would cost: an `Arc` of a copy of every column and index;
//! 4. an InfoSchema v2 full load of the same `TableInfo`s (name / ID items, the table
//!    cache with the table objects); the model itself is moved in, not counted.
//!
//! The numbers are printed (`--nocapture`) and recorded in spec N13 §5.

mod common;

use std::sync::Arc;
use std::sync::atomic::Ordering;

use futures::executor::block_on;
use tidb_infoschema::autoid::Allocators;
use tidb_infoschema::infoschema_v2::Ctx;
use tidb_infoschema::tables::table_from_meta;
use tidb_infoschema::{new_builder, new_data};
use tidb_model::{ColumnInfo, IndexInfo, TableInfo};

use common::{db_info, table_info};

#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

/// jemalloc `stats.allocated` (bytes allocated by the application), refreshed.
fn live() -> isize {
    tikv_jemalloc_ctl::epoch::advance().unwrap();
    tikv_jemalloc_ctl::stats::allocated::read().unwrap() as isize
}

/// The columns and indices of one table, copied.
type Copies = (Vec<Arc<ColumnInfo>>, Vec<Arc<IndexInfo>>);

const TABLES: usize = 10_000;
const COLS: usize = 30;
const IDXS: usize = 3;

#[test]
fn arc_sharing_memory_10k_tables() {
    let base = live();
    let infos: Vec<Arc<TableInfo>> = (0..TABLES)
        .map(|i| Arc::new(table_info(100 + i as i64, COLS, IDXS)))
        .collect();
    let model = live() - base;

    // 2. Table objects.
    let before = live();
    let tables: Vec<_> = infos
        .iter()
        .map(|t| table_from_meta(Allocators::default(), Arc::clone(t)).unwrap())
        .collect();
    let objects = live() - before;
    for (t, info) in tables.iter().zip(&infos) {
        for (c, ci) in t.cols().iter().zip(info.columns.iter().flatten()) {
            assert!(Arc::ptr_eq(&c.info, ci));
        }
        for (i, ii) in t.indices().iter().zip(info.indices.iter().flatten()) {
            assert!(Arc::ptr_eq(i.meta(), ii));
        }
    }

    // 3. The per-object copies the table objects made before #229.
    let before = live();
    let copies: Vec<Copies> = infos
        .iter()
        .map(|t| {
            (
                t.columns
                    .iter()
                    .flatten()
                    .map(|c| Arc::new(ColumnInfo::clone(c)))
                    .collect(),
                t.indices
                    .iter()
                    .flatten()
                    .map(|i| Arc::new(IndexInfo::clone(i)))
                    .collect(),
            )
        })
        .collect();
    let copy = live() - before;
    drop(copies);
    drop(tables);

    // 4. InfoSchema v2 full load (unbounded table cache: every table object stays).
    tidb_sessionctx::variable::tidb_vars::SchemaCacheSize.store(0, Ordering::SeqCst);
    let mut db = db_info();
    let before = live();
    // The decoded TableInfos are moved in (shared, as Go's `[]*TableInfo`).
    db.deprecated.tables = Some(infos);
    let data = new_data();
    let mut b = new_builder(None, None, Arc::clone(&data));
    block_on(b.init_with_db_infos(vec![db], vec![], vec![], 1)).unwrap();
    let is = block_on(b.build(1));
    let full_load = live() - before;
    let t = block_on(is.table_by_id(Ctx::background(), 100)).unwrap();
    for (c, ci) in t.cols().iter().zip(t.meta().columns.iter().flatten()) {
        assert!(Arc::ptr_eq(&c.info, ci));
    }

    let ncols = (TABLES * COLS) as f64;
    let mib = |b: isize| b as f64 / (1 << 20) as f64;
    eprintln!(
        "arc_sharing_mem: {TABLES} tables x {COLS} columns x {IDXS} indices\n  \
         model (TableInfo):            {:8.1} MiB ({:.0} B/column)\n  \
         table objects (shared):       {:8.1} MiB ({:.0} B/column incl. per-table parts)\n  \
         copies before #229 (removed): {:8.1} MiB ({:.0} B/column incl. indices)\n  \
         InfoSchema v2 full load:      {:8.1} MiB (name / ID items, cached table objects; the model is moved in)",
        mib(model),
        model as f64 / ncols,
        mib(objects),
        objects as f64 / ncols,
        mib(copy),
        copy as f64 / ncols,
        mib(full_load),
    );
    // Sharing: per column, a table object holds its `table.Column` wrapper (an `Arc` of
    // the ColumnInfo plus the generated / default expression slots, in an `Arc`) and the
    // entries of its column lists, but no ColumnInfo: less than one ColumnInfo struct
    // (which a copy would allocate, plus its heap parts).
    let col_info = std::mem::size_of::<ColumnInfo>() as f64;
    let wrapper = std::mem::size_of::<tidb_infoschema::table::column::Column>() as f64;
    eprintln!("  size_of: ColumnInfo {col_info} B, table::Column {wrapper} B");
    assert!(
        (objects as f64 / ncols) < col_info,
        "table objects copy the columns: {objects} B for {ncols} columns"
    );
    assert!(copy as f64 / ncols > col_info);
}
