//! go-file: pkg/meta/model/index.go

#![allow(non_upper_case_globals)]

use std::collections::HashSet;
use std::sync::LazyLock;

use tidb_base::Error;
use tidb_parser::ast::{Expr, Stmt};
use tidb_parser::model::CIStr;
use tidb_parser::types::{FieldType, UnspecifiedLength};

use crate::column::sl;
use crate::{
    ColumnInfo, DistanceMetric, DistanceMetricCosine, DistanceMetricL2, IndexColumn, IndexInfo,
    StatePublic, TableInfo, changingIndexPrefix, removingObjPrefix,
};

/// IndexableFnNameToDistanceMetric is a map of function name to distance metric.
pub static IndexableFnNameToDistanceMetric: LazyLock<Vec<(&'static str, DistanceMetric)>> =
    LazyLock::new(|| {
        vec![
            (
                tidb_parser::ast_ddl::VecCosineDistance,
                DistanceMetricCosine,
            ),
            (tidb_parser::ast_ddl::VecL2Distance, DistanceMetricL2),
        ]
    });

/// IndexableDistanceMetricToFnName is a map of distance metric to function name.
pub static IndexableDistanceMetricToFnName: LazyLock<Vec<(DistanceMetric, &'static str)>> =
    LazyLock::new(|| {
        vec![
            (
                DistanceMetricCosine,
                tidb_parser::ast_ddl::VecCosineDistance,
            ),
            (DistanceMetricL2, tidb_parser::ast_ddl::VecL2Distance),
        ]
    });

/// GenUniqueChangingIndexName generates a unique changing index name.
// go: GenUniqueChangingIndexName L1
pub fn gen_unique_changing_index_name(tbl_info: &TableInfo, idx_info: &IndexInfo) -> String {
    // Check whether the new index name is used.
    let index_name_map: HashSet<&str> = sl(&tbl_info.indices)
        .iter()
        .map(|i| i.name.l.as_str())
        .collect();
    let mut suffix = 0;
    let mut new_index_name = format!("{changingIndexPrefix}{}_{suffix}", idx_info.name.o);
    while index_name_map.contains(tidb_base::gostd::to_lower(&new_index_name).as_str()) {
        suffix += 1;
        new_index_name = format!("{changingIndexPrefix}{}_{suffix}", idx_info.name.o);
    }
    new_index_name
}

impl Clone for IndexInfo {
    /// Clone clones IndexInfo: the columns are cloned (a nil list becomes empty); the
    /// affected columns are cloned when not nil.
    // go: IndexInfo.Clone L1
    fn clone(&self) -> IndexInfo {
        let mut ni = self.struct_copy();
        ni.columns = Some(sl(&self.columns).to_vec());
        if let Some(ac) = &self.affect_column {
            ni.affect_column = Some(ac.clone());
        }
        ni
    }
}

impl IndexInfo {
    /// IsChanging checks whether the index is changing.
    // go: IndexInfo.IsChanging L1
    pub fn is_changing(&self) -> bool {
        self.name.o.starts_with(changingIndexPrefix)
    }

    /// IsRemoving checks whether the index is removing.
    // go: IndexInfo.IsRemoving L1
    pub fn is_removing(&self) -> bool {
        self.name.o.starts_with(removingObjPrefix)
    }

    /// GetRemovingOriginName returns the original name of the removing index.
    // go: IndexInfo.GetRemovingOriginName L1
    pub fn get_removing_origin_name(&self) -> &str {
        self.name
            .o
            .strip_prefix(removingObjPrefix)
            .unwrap_or(&self.name.o)
    }

    /// GetChangingOriginName returns the original name of the changing index.
    // go: IndexInfo.GetChangingOriginName L1
    pub fn get_changing_origin_name(&self) -> &str {
        let idx_name = self
            .name
            .o
            .strip_prefix(changingIndexPrefix)
            .unwrap_or(&self.name.o);
        // Since the unique idxName may contain the suffix number (indexName_num), better trim
        // the suffix.
        match idx_name.rfind('_') {
            None => idx_name,
            Some(pos) => &idx_name[..pos],
        }
    }

    /// HasPrefixIndex returns whether any columns of this index uses prefix length.
    // go: IndexInfo.HasPrefixIndex L1
    pub fn has_prefix_index(&self) -> bool {
        sl(&self.columns)
            .iter()
            .any(|ic| ic.length != UnspecifiedLength)
    }

    /// HasColumnInIndexColumns checks whether the index contains the column with the
    /// specified ID. Panics if an index column's offset is out of range, as Go does.
    // go: IndexInfo.HasColumnInIndexColumns L1
    pub fn has_column_in_index_columns(&self, tbl_info: &TableInfo, col_id: i64) -> bool {
        sl(&self.columns)
            .iter()
            .any(|ic| sl(&tbl_info.columns)[ic.offset as usize].id == col_id)
    }

    /// FindColumnByName finds the index column with the specified name.
    // go: IndexInfo.FindColumnByName L1
    pub fn find_column_by_name(&self, name_l: &str) -> Option<&IndexColumn> {
        find_index_column_by_name(sl(&self.columns), name_l).1
    }

    /// IsPublic checks if the index state is public.
    // go: IndexInfo.IsPublic L1
    pub fn is_public(&self) -> bool {
        self.state == StatePublic
    }

    /// IsTiFlashLocalIndex checks whether the index is a TiFlash local index.
    // go: IndexInfo.IsTiFlashLocalIndex L1
    pub fn is_tiflash_local_index(&self) -> bool {
        self.vector_info.is_some()
    }

    /// HasCondition checks whether the index has a partial index condition.
    // go: IndexInfo.HasCondition L1
    pub fn has_condition(&self) -> bool {
        !self.condition_expr_string.is_empty()
    }

    /// ConditionExpr parses the partial index condition (`select <condition>`) and returns
    /// the expression. The AST is the parser's (D02.1, a hand-written subset); a condition
    /// outside that subset is a parse error here.
    // go: IndexInfo.ConditionExpr L2 (the ported parser and its AST)
    pub fn condition_expr(&self) -> Result<Expr, Error> {
        let stmt_str = format!("select {}", self.condition_expr_string);
        let out = tidb_parser::parse(&stmt_str, &tidb_parser::ParseOptions::default())
            .map_err(|e| Error::plain(format!("{e:?}")))?;
        match out.stmts.into_iter().next() {
            Some(Stmt::Select(sel)) => {
                let mut fields = sel.fields.fields;
                match fields.drain(..).next().and_then(|f| f.expr) {
                    Some(e) => Ok(e),
                    None => Err(Error::plain("no expression in the partial index condition")),
                }
            }
            _ => Err(Error::plain(
                "the partial index condition is not an expression",
            )),
        }
    }
}

/// FindIndexByColumns find IndexInfo in indices which is cover the specified columns.
// go: FindIndexByColumns L1
pub fn find_index_by_columns<'a>(
    tb_info: &TableInfo,
    indices: &'a [IndexInfo],
    cols: &[CIStr],
) -> Option<&'a IndexInfo> {
    indices
        .iter()
        .find(|index| is_index_prefix_covered(tb_info, index, cols))
}

/// IsIndexPrefixCovered checks the index's columns beginning with the cols.
// go: IsIndexPrefixCovered L1
pub fn is_index_prefix_covered(tb_info: &TableInfo, index: &IndexInfo, cols: &[CIStr]) -> bool {
    let icols = sl(&index.columns);
    let tcols = sl(&tb_info.columns);
    if icols.len() < cols.len() {
        return false;
    }
    for i in 0..cols.len() {
        if cols[i].l != icols[i].name.l || icols[i].offset >= tcols.len() as isize {
            return false;
        }
        let col_info = &tcols[icols[i].offset as usize];
        if icols[i].length != UnspecifiedLength && icols[i].length < col_info.get_flen() {
            return false;
        }
    }
    true
}

/// FindIndexByColumnsForForeignKey find IndexInfo in indices which is cover the specified
/// columns, for a foreign key.
// go: FindIndexByColumnsForForeignKey L1
pub fn find_index_by_columns_for_foreign_key<'a>(
    tb_info: &TableInfo,
    indices: &'a [IndexInfo],
    cols: &[CIStr],
) -> Option<&'a IndexInfo> {
    indices
        .iter()
        .find(|index| is_index_prefix_covered_for_foreign_key(tb_info, index, cols))
}

/// IsIndexPrefixCoveredForForeignKey checks the index's columns beginning with the cols and
/// the partial index condition is covered by the foreign key columns.
// go: IsIndexPrefixCoveredForForeignKey L1
pub fn is_index_prefix_covered_for_foreign_key(
    tb_info: &TableInfo,
    index: &IndexInfo,
    cols: &[CIStr],
) -> bool {
    if !is_index_prefix_covered(tb_info, index, cols) {
        return false;
    }
    is_index_condition_covered_by_foreign_key_cols(index, cols)
}

/// isIndexConditionCoveredByForeignKeyCols: the condition must be `col IS NOT NULL` on one
/// of the columns.
// go: isIndexConditionCoveredByForeignKeyCols L1
fn is_index_condition_covered_by_foreign_key_cols(index: &IndexInfo, cols: &[CIStr]) -> bool {
    if !index.has_condition() {
        return true;
    }
    let Ok(expr) = index.condition_expr() else {
        return false;
    };
    let Expr::IsNull(is_null_expr) = expr else {
        return false;
    };
    if !is_null_expr.not {
        return false;
    }
    let Expr::Column(col_expr) = &is_null_expr.expr else {
        return false;
    };
    let name_l = col_expr.name.l();
    cols.iter().any(|col| name_l == col.l)
}

/// FindIndexInfoByID finds IndexInfo in indices by id.
// go: FindIndexInfoByID L1
pub fn find_index_info_by_id(indices: &[IndexInfo], id: i64) -> Option<&IndexInfo> {
    indices.iter().find(|idx| idx.id == id)
}

impl Clone for IndexColumn {
    /// Clone clones IndexColumn (a struct copy).
    // go: IndexColumn.Clone L1
    fn clone(&self) -> IndexColumn {
        self.struct_copy()
    }
}

/// FindIndexColumnByName finds IndexColumn by name. When IndexColumn is not found, returns
/// (-1, None).
// go: FindIndexColumnByName L1
pub fn find_index_column_by_name<'a>(
    index_cols: &'a [IndexColumn],
    name_l: &str,
) -> (isize, Option<&'a IndexColumn>) {
    for (i, ic) in index_cols.iter().enumerate() {
        if ic.name.l == name_l {
            return (i as isize, Some(ic));
        }
    }
    (-1, None)
}

/// GetIdxChangingFieldType gets the field type of index column. Since both
/// `ChangingFieldType` and `FieldType` are used in index column, the index column needs to
/// use the changing type when `UseChangingType` is set.
// go: GetIdxChangingFieldType L1
pub fn get_idx_changing_field_type<'a>(
    idx_col: &IndexColumn,
    col: &'a ColumnInfo,
) -> &'a FieldType {
    if idx_col.use_changing_type
        && let Some(ft) = &col.changing_field_type
    {
        return ft;
    }
    &col.field_type
}
