//! go-file: pkg/executor/table_reader.go
//!
//! The table reader executor (block D12.1) comes with VS1-34. VS1-12 needs only
//! `buildVirtualColumnIndex`, shared with the point get.

use std::sync::Arc;

use tidb_expression::Schema;
use tidb_model::{ColumnInfo, find_column_info_by_id};

/// The schema positions of the virtual generated columns, in column definition order (the
/// order they are computed in).
///
/// Go calls `plannercore.FindColumnInfoByID` (D10.9, VS1-31); the model's function of the
/// same name has the same body (the first column with the ID).
// go: buildVirtualColumnIndex L1
pub fn build_virtual_column_index(schema: &Schema, columns: &[Arc<ColumnInfo>]) -> Vec<usize> {
    let mut virtual_column_index: Vec<usize> = Vec::with_capacity(columns.len());
    for (i, col) in schema.columns.iter().enumerate() {
        if col.virtual_expr.is_some() {
            virtual_column_index.push(i);
        }
    }
    let offset = |i: usize| {
        find_column_info_by_id(columns, schema.columns[i].id)
            .expect("a virtual column of the plan's columns")
            .offset
    };
    // slices.SortFunc: not stable; the offsets of distinct columns differ.
    virtual_column_index.sort_by_key(|&i| offset(i));
    virtual_column_index
}

/// The schema the table reader executor is built with: the reader's, or its table plan's when
/// the reader has none. Go writes the table plan's schema into the plan
/// (`v.SetSchema(v.GetTablePlan().Schema())`, upstream `#50358`); the plan may be the one in
/// the instance plan cache, shared by every session, so the executor reads the schema
/// instead (the plan-cache exception A3 spec §2.3 row 9b). `buildTableReader` (D12.1, VS1-34)
/// builds the executor and its result columns from this schema.
// go-part: executorBuilder.buildTableReader (the empty-schema fix of upstream #50358, read instead of written: A3 spec §2.3 row 9b)
pub fn table_reader_schema(v: &tidb_planner::core::physical_plans::PhysicalTableReader) -> &Schema {
    use tidb_planner::PhysicalPlan;
    if v.schema().columns.is_empty()
        && let Some(tp) = v.get_table_plan()
        && !tp.schema().columns.is_empty()
    {
        return tp.schema();
    }
    v.schema()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tidb_expression::{Column, new_schema};
    use tidb_planner::core::physical_plans::{PhysicalTableReader, PhysicalTableScan};
    use tidb_planner::core::plan_cache_heap_size::{plan_structure_hash, seal_plan};
    use tidb_planner::{PhysicalPlan, PhysicalPlanRef};

    use super::table_reader_schema;

    /// A3 spec §2.3 row 9b: a cached table reader whose schema is empty (Go's `#50358` case)
    /// gives the executor its table plan's schema, and the cached plan is unchanged
    /// (structural hash) and still has no schema of its own.
    #[test]
    fn empty_reader_schema_is_read_from_the_table_plan_not_written() {
        let col = Arc::new(Column {
            unique_id: 1,
            ..Default::default()
        });
        let mut scan = PhysicalTableScan::default();
        scan.set_schema(new_schema(vec![col]));
        let scan: PhysicalPlanRef = Arc::new(scan);
        let mut reader = PhysicalTableReader::default();
        reader.set_schema(new_schema(Vec::new()));
        reader.set_children(vec![Arc::clone(&scan)]);
        let reader: PhysicalPlanRef = Arc::new(reader);
        seal_plan(reader.as_ref());
        let before = plan_structure_hash(reader.as_ref());

        let r = reader
            .as_any()
            .downcast_ref::<PhysicalTableReader>()
            .unwrap();
        assert_eq!(table_reader_schema(r).columns.len(), 1);
        assert!(r.schema().columns.is_empty());
        assert_eq!(plan_structure_hash(reader.as_ref()), before);
    }
}
