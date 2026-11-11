//! go-file: pkg/expression/schema.go
//!
//! `Schema` and `KeyInfo`. Columns are `Arc<Column>` (spec §3.1): Go shares `*Column`
//! between schemas and plans and clones before changing one.
//! `GetUsedList` compares virtual expressions with `ScalarFunction.Equal` and comes with
//! part 2.

use std::sync::Arc;

use tidb_model::ExtraHandleID;

use crate::column::Column;
use crate::expression::Expr;
use crate::size;

/// KeyInfo stores the columns of one unique key or primary key.
pub type KeyInfo = Vec<Arc<Column>>;

/// Clone copies the entire UniqueKey.
// go: KeyInfo.Clone L1
pub fn key_info_clone(ki: &KeyInfo) -> KeyInfo {
    ki.iter().map(|col| Arc::new((**col).clone())).collect()
}

/// String implements fmt.Stringer interface.
// go: KeyInfo.String L1 [M-NAME]
pub fn key_info_string(ki: &KeyInfo) -> Vec<u8> {
    let uk_col_strs: Vec<Vec<u8>> = ki.iter().map(|col| col.to_string()).collect();
    let mut out = b"[".to_vec();
    out.extend_from_slice(&uk_col_strs.join(&b","[..]));
    out.push(b']');
    out
}

/// Schema stands for the row schema and unique key information get from input.
#[derive(Clone, Debug, Default)]
pub struct Schema {
    /// Columns.
    pub columns: Vec<Arc<Column>>,
    /// Keys.
    pub keys: Vec<KeyInfo>,
    /// UniqueKeys stores those unique indexes that allow null values, but Keys does not
    /// allow null values. since equivalence conditions can filter out null values, in this
    /// case a unique index with null values can be a Key.
    pub unique_keys: Vec<KeyInfo>,
}

impl Schema {
    /// String implements fmt.Stringer interface.
    // go: Schema.String L1 [M-NAME]
    pub fn to_string(&self) -> Vec<u8> {
        let col_strs: Vec<Vec<u8>> = self.columns.iter().map(|col| col.to_string()).collect();
        let uk_strs: Vec<Vec<u8>> = self.keys.iter().map(key_info_string).collect();
        let mut out = b"Column: [".to_vec();
        out.extend_from_slice(&col_strs.join(&b","[..]));
        out.extend_from_slice(b"] Unique key: [");
        out.extend_from_slice(&uk_strs.join(&b","[..]));
        out.push(b']');
        out
    }

    /// Clone copies the total schema.
    // go: Schema.Clone L1
    pub fn clone_schema(&self) -> Schema {
        let cols: Vec<Arc<Column>> = self
            .columns
            .iter()
            .map(|col| Arc::new((**col).clone()))
            .collect();
        let keys: Vec<KeyInfo> = self.keys.iter().map(key_info_clone).collect();
        let mut schema = new_schema(cols);
        schema.set_keys(keys);
        schema
    }

    /// Equal checks if two schemas are equal.
    // go: Schema.Equal L1
    pub fn equal(&self, other: &Schema) -> bool {
        if self.columns.len() != other.columns.len() {
            return false;
        }
        for (i, col) in self.columns.iter().enumerate() {
            if !col.equal_col(&other.columns[i]) {
                return false;
            }
        }
        true
    }

    /// RetrieveColumn retrieves column in expression from the columns in schema.
    // go: Schema.RetrieveColumn L1
    pub fn retrieve_column(&self, col: &Column) -> Option<&Arc<Column>> {
        let index = self.column_index(col);
        if index != -1 {
            return Some(&self.columns[index as usize]);
        }
        None
    }

    /// IsUnique checks if the columns can identify a row in the schema. If strong is true,
    /// only Keys are considered; otherwise UniqueKeys are considered.
    // go: Schema.IsUnique L1
    pub fn is_unique(&self, strong: bool, cols: &[Arc<Column>]) -> bool {
        let slices_to_be_iterated = if strong {
            &self.keys
        } else {
            &self.unique_keys
        };
        for key in slices_to_be_iterated {
            if key.len() > cols.len() {
                continue;
            }
            let mut all_found = true;
            'next_key_col: for key_cols in key {
                for col in cols {
                    if key_cols.equal_col(col) {
                        continue 'next_key_col;
                    }
                }
                all_found = false;
                break;
            }
            if !all_found {
                continue;
            }
            return true;
        }
        false
    }

    /// ColumnIndex finds the index for a column.
    // go: Schema.ColumnIndex L1
    pub fn column_index(&self, col: &Column) -> isize {
        let mut backup_idx: isize = -1;
        for (i, c) in self.columns.iter().enumerate() {
            if c.unique_id == col.unique_id {
                backup_idx = i as isize;
                if c.is_prefix {
                    // instead of returning a prefix column
                    // prefer to find a full column
                    // only clustered index table can meet this:
                    //   same column `c1` maybe appear in both primary key and secondary index
                    // so secondary index itself can have two `c1` column one for indexKey and one for handle
                    continue;
                }
                return i as isize;
            }
        }
        backup_idx
    }

    /// Contains checks if the schema contains the column.
    // go: Schema.Contains L1
    pub fn contains(&self, col: &Column) -> bool {
        self.column_index(col) != -1
    }

    /// Len returns the number of columns in schema.
    // go: Schema.Len L1
    pub fn len(&self) -> usize {
        self.columns.len()
    }

    /// Whether the schema has no column.
    // glue: s.Len() == 0
    pub fn is_empty(&self) -> bool {
        self.columns.is_empty()
    }

    /// Append append new column to the columns stored in schema.
    // go: Schema.Append L1
    pub fn append(&mut self, cols: impl IntoIterator<Item = Arc<Column>>) {
        self.columns.extend(cols);
    }

    /// SetKeys sets the keys of the schema.
    // go: Schema.SetKeys L1
    pub fn set_keys(&mut self, keys: Vec<KeyInfo>) {
        self.keys = keys;
    }

    /// SetUniqueKeys sets the unique keys of the schema.
    // go: Schema.SetUniqueKeys L1
    pub fn set_unique_keys(&mut self, keys: Vec<KeyInfo>) {
        self.unique_keys = keys;
    }

    /// ColumnsIndices will return a slice which contains the position of each column in
    /// schema. If there is one column that doesn't match, nil will be returned.
    // go: Schema.ColumnsIndices L1
    pub fn columns_indices(&self, cols: &[Arc<Column>]) -> Option<Vec<isize>> {
        let mut ret = Vec::with_capacity(cols.len());
        for col in cols {
            let pos = self.column_index(col);
            if pos == -1 {
                return None;
            }
            ret.push(pos);
        }
        Some(ret)
    }

    /// ColumnsByIndices returns columns by multiple offsets. Callers should guarantee that
    /// all the offsets provided should be valid, which means offset should: 1. not smaller
    /// than 0, and 2. not exceed len(s.Columns)
    // go: Schema.ColumnsByIndices L1
    pub fn columns_by_indices(&self, offsets: &[isize]) -> Vec<Arc<Column>> {
        offsets
            .iter()
            .map(|&o| Arc::clone(&self.columns[o as usize]))
            .collect()
    }

    /// ExtractColGroups checks if column groups are from current schema, and returns
    /// offsets of those satisfied column groups.
    // go: Schema.ExtractColGroups L1
    pub fn extract_col_groups(
        &self,
        col_groups: &[Vec<Arc<Column>>],
    ) -> (Option<Vec<Vec<isize>>>, Option<Vec<usize>>) {
        if col_groups.is_empty() {
            return (None, None);
        }
        let mut extracted = Vec::with_capacity(col_groups.len());
        let mut offsets = Vec::with_capacity(col_groups.len());
        for (i, g) in col_groups.iter().enumerate() {
            if let Some(j) = self.columns_indices(g) {
                extracted.push(j);
                offsets.push(i);
            }
        }
        (Some(extracted), Some(offsets))
    }

    /// MemoryUsage return the memory usage of Schema
    // go: Schema.MemoryUsage L2 (unsafe.Sizeof(Schema{}) is Go's size on 64-bit targets; capacities are the lengths Go's make/append give for these slices, spec §4)
    pub fn memory_usage(&self) -> i64 {
        let mut sum = emptySchemaSize
            + self.columns.capacity() as i64 * size::SizeOfPointer
            + (self.keys.capacity() + self.unique_keys.capacity()) as i64 * size::SizeOfSlice;

        for col in &self.columns {
            sum += col.memory_usage();
        }
        for cols in &self.keys {
            sum += cols.capacity() as i64 * size::SizeOfPointer;
            for col in cols {
                sum += col.memory_usage();
            }
        }
        for cols in &self.unique_keys {
            sum += cols.capacity() as i64 * size::SizeOfPointer;
            for col in cols {
                sum += col.memory_usage();
            }
        }
        sum
    }

    /// GetExtraHandleColumn gets the extra handle column.
    // go: Schema.GetExtraHandleColumn L1
    pub fn get_extra_handle_column(&self) -> Option<&Arc<Column>> {
        let column_len = self.columns.len();
        if column_len > 0 && self.columns[column_len - 1].id == ExtraHandleID as i64 {
            return Some(&self.columns[column_len - 1]);
        } else if column_len > 1 && self.columns[column_len - 2].id == ExtraHandleID as i64 {
            return Some(&self.columns[column_len - 2]);
        }
        None
    }
}

/// `unsafe.Sizeof(Schema{})` of Go v8.5.8 on amd64/arm64.
#[allow(non_upper_case_globals)]
const emptySchemaSize: i64 = 72;

/// ExprReferenceSchema checks if any column of this expression are from the schema.
// go: ExprReferenceSchema L1
pub fn expr_reference_schema(expr: &Expr, schema: &Schema) -> bool {
    match expr {
        Expr::Column(v) => schema.contains(v),
        Expr::ScalarFunction(v) => {
            for arg in v.get_args() {
                if expr_reference_schema(arg, schema) {
                    return true;
                }
            }
            false
        }
        Expr::CorrelatedColumn(_) | Expr::Constant(_) => false,
    }
}

/// ExprFromSchema checks if all columns of this expression are from the same schema.
// go: ExprFromSchema L1
pub fn expr_from_schema(expr: &Expr, schema: &Schema) -> bool {
    match expr {
        Expr::Column(v) => schema.contains(v),
        Expr::ScalarFunction(v) => {
            for arg in v.get_args() {
                if !expr_from_schema(arg, schema) {
                    return false;
                }
            }
            true
        }
        Expr::CorrelatedColumn(_) | Expr::Constant(_) => true,
    }
}

/// MergeSchema will merge two schema into one schema. We shouldn't need to consider unique
/// keys. That will be processed in build_keyinfo.go.
// go: MergeSchema L1
pub fn merge_schema(l_schema: Option<&Schema>, r_schema: Option<&Schema>) -> Option<Schema> {
    match (l_schema, r_schema) {
        (None, None) => None,
        (None, Some(r)) => Some(r.clone_schema()),
        (Some(l), None) => Some(l.clone_schema()),
        (Some(l), Some(r)) => {
            let tmp_l = l.clone_schema();
            let tmp_r = r.clone_schema();
            let mut cols = tmp_l.columns;
            cols.extend(tmp_r.columns);
            Some(new_schema(cols))
        }
    }
}

/// NewSchema returns a schema made by its parameter.
// go: NewSchema L1
pub fn new_schema(cols: Vec<Arc<Column>>) -> Schema {
    Schema {
        columns: cols,
        keys: Vec::new(),
        unique_keys: Vec::new(),
    }
}
