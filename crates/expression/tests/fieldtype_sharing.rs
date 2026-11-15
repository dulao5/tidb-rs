//! Heap allocations of the planner's expression-schema path for one 30-column table
//! (spec N13 §8, issue #229 batch A): how many allocations a `FieldType` copy costs on the
//! way from `TableInfo` to an expression schema and its clones.
//!
//! A counting allocator (this binary has one test, so nothing else allocates concurrently)
//! counts the allocations of each step:
//!
//! 1. `TableInfo2SchemaAndNames` (Go `RetType: col.FieldType.Clone()`, one copy per column);
//! 2. `Schema.Clone` (Go `col.Clone()`: a new `Column` sharing `RetType`);
//! 3. `Column.Clone` of every column (Go shares `RetType`);
//! 4. the executor's return types (Go `retFieldTypes[i] = col.RetType`, shared): the
//!    `ret_type` of every column taken as the executor does.
//!
//! The numbers are printed (`--nocapture`) and recorded in spec N13 §8.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tidb_base::mysql;
use tidb_expression::*;
use tidb_model::{ColumnInfo, StatePublic, TableInfo};
use tidb_parser::model::CIStr;
use tidb_parser::types::FieldType;
use tidb_sessionctx::exprctx::{BuildContextRef, new_simple_plan_column_id_allocator};
use tidb_sessionctx::exprstatic as es;

struct Counting;

static ALLOCS: AtomicUsize = AtomicUsize::new(0);
static BYTES: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        // SAFETY: forwards the caller's layout to the system allocator.
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` was allocated by `alloc` above with this layout.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// Allocations and bytes of `f`.
fn count<T>(f: impl FnOnce() -> T) -> (T, usize, usize) {
    let (a, b) = (ALLOCS.load(Ordering::SeqCst), BYTES.load(Ordering::SeqCst));
    let r = f();
    (
        r,
        ALLOCS.load(Ordering::SeqCst) - a,
        BYTES.load(Ordering::SeqCst) - b,
    )
}

const COLS: usize = 30;

/// A public table of `COLS` columns, alternately BIGINT (binary) and VARCHAR(64) utf8mb4.
fn table() -> TableInfo {
    let columns = (0..COLS)
        .map(|i| {
            let mut ft;
            if i % 2 == 0 {
                ft = FieldType::new(mysql::TypeLonglong);
                ft.set_flen(20);
                ft.set_charset("binary");
                ft.set_collate("binary");
            } else {
                ft = FieldType::new(mysql::TypeVarchar);
                ft.set_flen(64);
                ft.set_charset("utf8mb4");
                ft.set_collate("utf8mb4_bin");
            }
            Arc::new(ColumnInfo {
                id: i as i64 + 1,
                name: CIStr::new(&format!("c{i}")),
                offset: i as isize,
                field_type: ft,
                state: StatePublic,
                version: 2,
                ..Default::default()
            })
        })
        .collect();
    TableInfo {
        id: 100,
        name: CIStr::new("t"),
        charset: "utf8mb4".into(),
        collate: "utf8mb4_bin".into(),
        columns: Some(columns),
        state: StatePublic,
        version: 5,
        max_column_id: COLS as i64,
        ..Default::default()
    }
}

#[test]
fn fieldtype_copies_on_the_expression_schema_path() {
    let tbl = table();
    let bctx: BuildContextRef = es::new_expr_context(vec![es::with_column_id_allocator(Arc::new(
        new_simple_plan_column_id_allocator(0),
    ))]);
    let db = CIStr::new("test");

    let ((schema, _names), build_a, build_b) =
        count(|| table_info_to_schema_and_names(&bctx, &db, &tbl).unwrap());
    let (cloned, clone_a, clone_b) = count(|| schema.clone_schema());
    let (cols, col_a, col_b) = count(|| {
        schema
            .columns
            .iter()
            .map(|c| Column::clone(c))
            .collect::<Vec<_>>()
    });
    let (ret_types, ret_a, ret_b) = count(|| {
        schema
            .columns
            .iter()
            .map(|c| Arc::clone(&c.ret_type))
            .collect::<Vec<_>>()
    });
    assert_eq!(cloned.columns.len(), COLS);
    assert_eq!(cols.len(), COLS);
    assert_eq!(ret_types.len(), COLS);
    eprintln!(
        "fieldtype_sharing: {COLS}-column table (allocations / bytes)\n  \
         TableInfo2SchemaAndNames: {build_a:5} / {build_b:7}\n  \
         Schema.Clone:             {clone_a:5} / {clone_b:7}\n  \
         Column.Clone x {COLS}:        {col_a:5} / {col_b:7}\n  \
         executor ret types:       {ret_a:5} / {ret_b:7}",
    );
}
