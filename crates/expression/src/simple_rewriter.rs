//! go-file: pkg/expression/simple_rewriter.go
//!
//! The name lookups of `simple_rewriter.go` that the planner's expression rewriter (D09.2)
//! uses (`FindFieldName`, `FindFieldNameIdxByColName`); the rest of the file
//! (`ParseSimpleExpr*`, the `BuildOption`s) comes with `buildSimpleExpr`.

use std::sync::Arc;

use tidb_base::Error;
use tidb_parser::ast;
use tidb_types::field_name::FieldName;

use crate::errors::errNonUniq;

/// `ast.ColumnName.String()`: the lower-case `schema.table.name`, empty parts omitted.
// glue: ast.ColumnName.String (pkg/parser/ast, D02.2)
pub fn column_name_string(n: &ast::ColumnName) -> String {
    let mut result = n.name.l();
    if !n.table.o.is_empty() {
        result = format!("{}.{}", n.table.l(), result);
    }
    if !n.schema.o.is_empty() {
        result = format!("{}.{}", n.schema.l(), result);
    }
    result
}

/// `ast.ColumnName.OrigColName()`: the original-case `schema.table.name`; the schema only
/// when the table is there.
// glue: ast.ColumnName.OrigColName (pkg/parser/ast, D02.2)
pub fn column_name_orig_col_name(n: &ast::ColumnName) -> String {
    let mut ret = n.name.o.clone();
    if n.table.o.is_empty() {
        return ret;
    }
    ret = format!("{}.{}", n.table.o, ret);
    if n.schema.o.is_empty() {
        return ret;
    }
    format!("{}.{}", n.schema.o, ret)
}

/// FindFieldName finds column name from NameSlice.
// go: FindFieldName L1
pub fn find_field_name(
    names: &[Arc<FieldName>],
    ast_col: &ast::ColumnName,
) -> Result<isize, Error> {
    let (db_name, tbl_name, col_name) = (ast_col.schema.l(), ast_col.table.l(), ast_col.name.l());
    let mut idx: isize = -1;

    for (i, name) in names.iter().enumerate() {
        // Early continue if not usable or column name doesn't match
        if name.not_explicit_usable || name.col_name.l != col_name {
            continue;
        }

        // Check database and table name match
        if (db_name.is_empty() || db_name == name.db_name.l)
            && (tbl_name.is_empty() || tbl_name == name.tbl_name.l)
        {
            if idx != -1 {
                // Do not allow multiple non-redundant columns with the same name.
                if names[idx as usize].redundant || name.redundant {
                    if !name.redundant {
                        idx = i as isize;
                    }
                    continue;
                }
                return Err(errNonUniq.gen_with_stack_by_args(tidb_base::go_args![
                    column_name_string(ast_col),
                    "field list"
                ]));
            }
            idx = i as isize;
        }
    }

    Ok(idx)
}

/// FindFieldNameIdxByColName finds the index of corresponding name in the given slice. -1 for not found.
// go: FindFieldNameIdxByColName L1
pub fn find_field_name_idx_by_col_name(names: &[Arc<FieldName>], col_name: &str) -> isize {
    for (i, name) in names.iter().enumerate() {
        if name.col_name.l == col_name {
            return i as isize;
        }
    }
    -1
}
