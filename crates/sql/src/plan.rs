//! Name resolution and physical planning of single-table SELECTs.
//!
//! A plan is a fixed pipeline, which covers every supported statement:
//!
//! ```text
//! source (Dual | PointGet | TableReader[cop: scan -> partial agg | limit])
//!   -> [root HashAgg (final over pushed partials, or complete)]
//!   -> [Sort] -> [Limit] -> projection
//! ```
//!
//! The shapes follow what Go TiDB picks for the same statements (PointGet for a single handle,
//! TableReader with a pushed partial aggregation and a root final HashAgg, root Sort), and
//! the result column metadata follows Go's name resolution (see [`crate::types`]).

use std::fmt;
use std::sync::Arc;

use tidb_distsql::AggFunc;
use tidb_meta::InfoSchema;
use tidb_meta::model::ColumnInfo;
use tidb_meta::model::FieldType;
use tidb_meta::model::TableInfo;
use tidb_meta::model::flag;
use tidb_meta::model::tp;
use tidb_mysql_protocol::ColumnDef;
use tidb_mysql_protocol::SqlError;

use crate::ir::AggName;
use crate::ir::ByItem;
use crate::ir::CmpOp;
use crate::ir::ColumnName;
use crate::ir::Expr;
use crate::ir::Field;
use crate::ir::Literal;
use crate::ir::Select;
use crate::ranges::Interval;
use crate::ranges::eval_const;
use crate::ranges::literal_num;
use crate::types::FieldNames;
use crate::types::column_def;

/// Session state the planner needs.
#[derive(Clone, Copy, Debug)]
pub struct PlanContext<'a> {
    /// Current database as the user wrote it in `USE` (Go `CurrentDB`).
    pub current_db: Option<&'a str>,
    /// Parameter values of a prepared statement execution.
    pub params: &'a [Literal],
    /// Prepare time: parameter values are unknown. Only the result columns are needed; the
    /// source is `Dual` and `LIMIT ?` is not evaluated.
    pub describe_only: bool,
}

/// Aggregate function kinds at the root.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AggKind {
    Count,
    Sum,
    Min,
    Max,
    FirstRow,
}

impl AggKind {
    fn name(self) -> &'static str {
        match self {
            AggKind::Count => "count",
            AggKind::Sum => "sum",
            AggKind::Min => "min",
            AggKind::Max => "max",
            AggKind::FirstRow => "firstrow",
        }
    }
}

/// One aggregate function evaluated at the root.
#[derive(Clone, Debug, PartialEq)]
pub struct RootAggFunc {
    pub kind: AggKind,
    /// Position of the argument in the input row.
    pub arg: usize,
    /// Type of the argument (in final mode: of the partial result).
    pub arg_type: FieldType,
    /// Result type (Go `baseFuncDesc.RetTp`).
    pub ret_type: FieldType,
}

/// Whether the root aggregation merges pushed partial results or aggregates raw rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AggMode {
    /// Input rows are TiKV partial results: `[func results..., group-by values...]`.
    Final,
    /// Input rows are scanned rows.
    Complete,
}

/// Root hash aggregation. Output rows hold one value per function.
#[derive(Clone, Debug, PartialEq)]
pub struct RootAgg {
    pub mode: AggMode,
    pub funcs: Vec<RootAggFunc>,
    /// Positions of the group-by values in the input row.
    pub group_by: Vec<usize>,
    pub group_types: Vec<FieldType>,
}

/// Partial aggregation pushed to TiKV (offsets into the scanned columns).
#[derive(Clone, Debug, PartialEq)]
pub struct PushedAgg {
    pub funcs: Vec<AggFunc>,
    pub group_by: Vec<usize>,
}

/// Where rows come from.
#[derive(Clone, Debug, PartialEq)]
pub enum Source {
    /// No rows (empty handle range, or prepare-time description).
    Dual,
    /// A single row by handle (KV point get).
    PointGet { handle: i64 },
    /// A coprocessor read of the inclusive handle range `[lo, hi]`.
    TableReader {
        lo: i64,
        hi: i64,
        desc: bool,
        keep_order: bool,
        /// Limit applied by each cop task (before any root operator).
        limit: Option<u64>,
        agg: Option<PushedAgg>,
    },
}

/// A sort key over the current row layout.
#[derive(Clone, Debug, PartialEq)]
pub struct SortKey {
    pub col: usize,
    pub desc: bool,
    pub ft: FieldType,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LimitSpec {
    pub offset: u64,
    pub count: u64,
}

/// A physical plan of a single-table SELECT.
#[derive(Clone, Debug)]
pub struct Plan {
    pub table: Arc<TableInfo>,
    /// Scanned columns, as indexes into `table.columns`.
    pub scan_columns: Vec<usize>,
    pub source: Source,
    pub agg: Option<RootAgg>,
    pub sort: Vec<SortKey>,
    pub limit: Option<LimitSpec>,
    /// Output columns: positions in the row after aggregation (or in the scanned row).
    pub output: Vec<usize>,
    /// Types of the output columns.
    pub output_types: Vec<FieldType>,
    /// Result-set metadata.
    pub columns: Vec<ColumnDef>,
}

impl Plan {
    /// The scanned columns' infos, in scan order.
    pub fn scan_column_infos(&self) -> Vec<&ColumnInfo> {
        self.scan_columns
            .iter()
            .map(|&i| &self.table.columns[i])
            .collect()
    }
}

fn unsupported(what: impl fmt::Display) -> SqlError {
    SqlError::not_supported(what)
}

/// `ER_BAD_FIELD_ERROR` (1054).
fn unknown_column(col: &str, clause: &str) -> SqlError {
    SqlError::new(1054, format!("Unknown column '{col}' in '{clause}'"))
}

/// The table a statement reads, resolved.
struct Resolved<'a> {
    table: Arc<TableInfo>,
    /// Database as shown in metadata (typed schema, else the current database).
    db: String,
    /// `TblName` of the normal planner path: alias, else the stored table name.
    tbl_as_name: String,
    /// `TblName` of Go's PointGet fast path: alias, else the table name as written.
    fast_tbl_name: String,
    sel: &'a Select,
    cx: PlanContext<'a>,
}

/// A select-list item after wildcard expansion.
#[derive(Clone, Debug)]
struct Item {
    kind: ItemKind,
    names: FieldNames,
    /// Alias (empty if none), for ORDER BY resolution.
    alias: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ItemKind {
    /// A table column (index into `table.columns`).
    Col(usize),
    /// An aggregate over a column; `None` is `COUNT(*)` / `COUNT(const)`.
    Agg(AggName, Option<usize>),
}

/// Plan a SELECT with a FROM clause.
pub fn plan_select(sel: &Select, is: &InfoSchema, cx: PlanContext<'_>) -> Result<Plan, SqlError> {
    let Some(from) = &sel.from else {
        return Err(unsupported("SELECT without FROM in the planner"));
    };
    let db = if !from.schema.is_empty() {
        from.schema.clone()
    } else {
        cx.current_db
            .filter(|d| !d.is_empty())
            .ok_or_else(|| SqlError::new(1046, "No database selected"))?
            .to_owned()
    };
    let table = is
        .table_by_name(&db, &from.name)
        .ok_or_else(|| SqlError::new(1146, format!("Table '{db}.{}' doesn't exist", from.name)))?;
    table
        .check_int_handle_readable()
        .map_err(|e| unsupported(e.to_string()))?;
    let r = Resolved {
        tbl_as_name: if from.alias.is_empty() {
            table.name.o.clone()
        } else {
            from.alias.clone()
        },
        fast_tbl_name: if from.alias.is_empty() {
            from.name.clone()
        } else {
            from.alias.clone()
        },
        table,
        db,
        sel,
        cx,
    };
    r.plan()
}

impl Resolved<'_> {
    fn column(&self, idx: usize) -> &ColumnInfo {
        &self.table.columns[idx]
    }

    /// Resolve a column reference against the table (Go: `findColumnNameByName`).
    fn resolve_column(&self, c: &ColumnName, clause: &str) -> Result<usize, SqlError> {
        let not_found = || unknown_column(&c.orig_col_name(), clause);
        if !c.schema.is_empty() && !c.schema.eq_ignore_ascii_case(&self.db) {
            return Err(not_found());
        }
        if !c.table.is_empty() && c.table.to_lowercase() != self.tbl_as_name.to_lowercase() {
            return Err(not_found());
        }
        let lower = c.name.to_lowercase();
        self.table
            .columns
            .iter()
            .position(|col| col.is_public() && col.name.l == lower)
            .ok_or_else(not_found)
    }

    fn col_names(&self, idx: usize, as_name: &str) -> FieldNames {
        let col = self.column(idx);
        FieldNames {
            schema: self.db.clone(),
            table: self.tbl_as_name.clone(),
            org_table: self.table.name.o.clone(),
            name: if as_name.is_empty() {
                col.name.o.clone()
            } else {
                as_name.to_owned()
            },
            org_name: col.name.o.clone(),
        }
    }

    /// Expand the select list (Go: `unfoldWildStar` + `buildProjectionField`).
    fn items(&self) -> Result<Vec<Item>, SqlError> {
        let mut items = Vec::new();
        for f in &self.sel.fields {
            match f {
                Field::Wildcard { schema, table } => {
                    let schema_ok = schema.is_empty() || schema.eq_ignore_ascii_case(&self.db);
                    let table_ok =
                        table.is_empty() || table.to_lowercase() == self.tbl_as_name.to_lowercase();
                    if !schema_ok || !table_ok {
                        let name = if schema.is_empty() {
                            table.clone()
                        } else {
                            format!("{schema}.{table}")
                        };
                        return Err(SqlError::new(1051, format!("Unknown table '{name}'")));
                    }
                    let mut cols: Vec<(usize, &ColumnInfo)> =
                        self.table.columns.iter().enumerate().collect();
                    cols.sort_by_key(|(_, c)| c.offset);
                    for (idx, col) in cols {
                        if col.is_public() && !col.hidden {
                            items.push(Item {
                                kind: ItemKind::Col(idx),
                                names: self.col_names(idx, ""),
                                alias: String::new(),
                            });
                        }
                    }
                }
                Field::Expr { expr, alias, text } => match expr {
                    Expr::Column(c) => {
                        let idx = self.resolve_column(c, "field list")?;
                        let as_name = if alias.is_empty() { &c.name } else { alias };
                        items.push(Item {
                            kind: ItemKind::Col(idx),
                            names: self.col_names(idx, as_name),
                            alias: alias.clone(),
                        });
                    }
                    Expr::Agg { name, arg } => {
                        let arg = match (&**arg, name) {
                            (Expr::Column(c), _) => Some(self.resolve_column(c, "field list")?),
                            (Expr::Literal(l), AggName::Count) if *l != Literal::Null => None,
                            _ => {
                                return Err(unsupported(
                                    "aggregate arguments other than a column (or COUNT(*))",
                                ));
                            }
                        };
                        items.push(Item {
                            kind: ItemKind::Agg(*name, arg),
                            names: FieldNames {
                                name: if alias.is_empty() {
                                    text.clone()
                                } else {
                                    alias.clone()
                                },
                                ..FieldNames::default()
                            },
                            alias: alias.clone(),
                        });
                    }
                    _ => {
                        return Err(unsupported(format!(
                            "'{text}' in the select list of a table read (only columns and \
                             COUNT/SUM/MIN/MAX are supported)"
                        )));
                    }
                },
            }
        }
        // Go: `MaxAliasIdentifierLen` (256) truncation of the column alias.
        for it in &mut items {
            truncate_name(&mut it.names.name);
        }
        Ok(items)
    }

    /// The handle column: the integer primary key of a `pk_is_handle` table.
    fn handle_column(&self) -> Option<usize> {
        if !self.table.pk_is_handle {
            return None;
        }
        self.table
            .columns
            .iter()
            .position(|c| c.field_type.has_flag(flag::PRI_KEY))
    }

    /// The handle range selected by WHERE, `None` if empty.
    fn handle_range(&self) -> Result<Option<(i64, i64)>, SqlError> {
        let Some(w) = &self.sel.where_ else {
            return Ok(Some((i64::MIN, i64::MAX)));
        };
        let mut conjuncts = Vec::new();
        flatten_and(w, &mut conjuncts);
        let mut comparisons: Vec<(&ColumnName, CmpOp, &Expr)> = Vec::new();
        for c in conjuncts {
            match c {
                Expr::Cmp(op, l, r) => match (&**l, &**r) {
                    (Expr::Column(col), v) if is_const(v) => comparisons.push((col, *op, v)),
                    (v, Expr::Column(col)) if is_const(v) => {
                        comparisons.push((col, op.flip(), v));
                    }
                    _ => {
                        return Err(unsupported(
                            "WHERE conditions other than comparisons of the integer primary key \
                             with constants",
                        ));
                    }
                },
                Expr::Between { expr, low, high } => match &**expr {
                    Expr::Column(col) if is_const(low) && is_const(high) => {
                        comparisons.push((col, CmpOp::Ge, low));
                        comparisons.push((col, CmpOp::Le, high));
                    }
                    _ => {
                        return Err(unsupported(
                            "BETWEEN other than on the integer primary key with constant bounds",
                        ));
                    }
                },
                _ => {
                    return Err(unsupported(
                        "WHERE conditions other than comparisons of the integer primary key \
                         with constants",
                    ));
                }
            }
        }
        let handle = self.handle_column();
        let mut range = Interval::full();
        for (col, op, v) in comparisons {
            let idx = self.resolve_column(col, "where clause")?;
            if Some(idx) != handle {
                return Err(unsupported(format!(
                    "conditions on column '{}' (only the integer primary key handle can be \
                     filtered)",
                    self.column(idx).name.o
                )));
            }
            if self.column(idx).field_type.is_unsigned() {
                return Err(unsupported("ranges on an UNSIGNED integer primary key"));
            }
            if self.cx.describe_only {
                continue;
            }
            let value = literal_num(&eval_const(v, self.cx.params)?)?;
            range = range.intersect(Interval::from_cmp(op, value));
        }
        if self.cx.describe_only {
            return Ok(None);
        }
        Ok(range.to_i64())
    }

    fn limit(&self) -> Result<Option<LimitSpec>, SqlError> {
        let Some(l) = &self.sel.limit else {
            return Ok(None);
        };
        if self.cx.describe_only {
            return Ok(None);
        }
        let get = |e: &Expr| -> Result<u64, SqlError> {
            match eval_const(e, self.cx.params)? {
                Literal::Int(i) if i >= 0 => Ok(i as u64),
                Literal::UInt(u) => Ok(u),
                _ => Err(SqlError::new(1210, "Incorrect arguments to LIMIT")),
            }
        };
        Ok(Some(LimitSpec {
            count: get(&l.count)?,
            offset: l.offset.as_ref().map(get).transpose()?.unwrap_or(0),
        }))
    }

    /// Whether Go would take its PointGet fast path (`TryFastPlan`), which names result
    /// columns slightly differently. Returns `Some(is_dual)` if it would.
    fn fast_point_get(&self) -> Option<bool> {
        let sel = self.sel;
        if !sel.order_by.is_empty() {
            return None;
        }
        if let Some(l) = &sel.limit {
            let count = eval_const(&l.count, self.cx.params).ok()?;
            let offset = match &l.offset {
                Some(o) => eval_const(o, self.cx.params).ok()?,
                None => Literal::Int(0),
            };
            let positive = |l: &Literal| match l {
                Literal::Int(i) => Some(*i),
                Literal::UInt(u) => i64::try_from(*u).ok(),
                _ => None,
            };
            if positive(&count)? <= 0 || positive(&offset)? != 0 {
                return None;
            }
        }
        if self
            .table
            .columns
            .iter()
            .any(|c| !c.is_public() || !c.generated_expr_string.is_empty())
        {
            return None;
        }
        let fast_l = self.fast_tbl_name.to_lowercase();
        for f in &sel.fields {
            match f {
                Field::Wildcard { table, .. } => {
                    if !table.is_empty() && table.to_lowercase() != fast_l {
                        return None;
                    }
                }
                Field::Expr {
                    expr: Expr::Column(c),
                    ..
                } => {
                    if !c.table.is_empty() && c.table.to_lowercase() != fast_l {
                        return None;
                    }
                    let lower = c.name.to_lowercase();
                    self.table.columns.iter().find(|col| col.name.l == lower)?;
                }
                _ => return None,
            }
        }
        // WHERE: a conjunction of `column = literal/?` with exactly one pair, on the handle.
        let mut conjuncts = Vec::new();
        flatten_and(sel.where_.as_ref()?, &mut conjuncts);
        let mut pairs = Vec::new();
        for c in conjuncts {
            let Expr::Cmp(CmpOp::Eq, l, r) = c else {
                return None;
            };
            let (col, v) = match (&**l, &**r) {
                (Expr::Column(c), v @ (Expr::Literal(_) | Expr::Param(_))) => (c, v),
                (v @ (Expr::Literal(_) | Expr::Param(_)), Expr::Column(c)) => (c, v),
                _ => return None,
            };
            let value = eval_const(v, self.cx.params).ok()?;
            if value == Literal::Null {
                return None;
            }
            if !col.table.is_empty() && col.table.to_lowercase() != fast_l {
                return None;
            }
            pairs.push((col.name.to_lowercase(), value));
        }
        let handle = self.column(self.handle_column()?);
        if pairs.len() != 1 || pairs[0].0 != handle.name.l {
            return None;
        }
        // Go converts the value to the column type: exact -> PointGet, overflow -> TableDual,
        // otherwise (e.g. 5.5) the normal planner is used.
        let num = literal_num(&pairs[0].1).ok()??;
        let (min, max) = int_type_range(&handle.field_type)?;
        let in_range = num.floor >= min && num.floor <= max;
        match (num.frac, in_range) {
            (_, false) => Some(true),
            (false, true) => Some(false),
            (true, true) => None,
        }
    }

    fn plan(&self) -> Result<Plan, SqlError> {
        let sel = self.sel;
        let mut items = self.items()?;

        // --- Aggregation shape ---
        let has_agg = items.iter().any(|i| matches!(i.kind, ItemKind::Agg(..)));
        let mut group_cols: Vec<usize> = Vec::new();
        for g in &sel.group_by {
            let idx = match &g.expr {
                Expr::Column(c) => self.resolve_column(c, "group statement")?,
                Expr::Position(p) => match usize::try_from(*p)
                    .ok()
                    .and_then(|p| p.checked_sub(1))
                    .and_then(|p| items.get(p))
                {
                    Some(Item {
                        kind: ItemKind::Col(idx),
                        ..
                    }) => *idx,
                    Some(_) => return Err(unsupported("GROUP BY an aggregate")),
                    None => return Err(unknown_column(&p.to_string(), "group statement")),
                },
                _ => return Err(unsupported("GROUP BY items other than columns")),
            };
            if g.desc {
                return Err(unsupported("GROUP BY ... DESC"));
            }
            if !group_cols.contains(&idx) {
                group_cols.push(idx);
            }
        }
        if sel.distinct {
            if !has_agg && group_cols.is_empty() {
                for it in &items {
                    if let ItemKind::Col(idx) = it.kind
                        && !group_cols.contains(&idx)
                    {
                        group_cols.push(idx);
                    }
                }
            } else if !group_cols.is_empty() {
                return Err(unsupported("DISTINCT together with GROUP BY"));
            }
            // DISTINCT over an aggregation without GROUP BY (one row) changes nothing.
        }
        let aggregated = has_agg || !group_cols.is_empty();
        if aggregated {
            for it in &items {
                if let ItemKind::Col(idx) = it.kind
                    && !group_cols.contains(&idx)
                {
                    return Err(unsupported(format!(
                        "non-aggregated column '{}' outside GROUP BY (only_full_group_by)",
                        self.column(idx).name.o
                    )));
                }
            }
        }

        // --- ORDER BY targets ---
        enum Target {
            Item(usize),
            Column(usize),
        }
        let mut order_targets = Vec::new();
        for ByItem { expr, desc } in &sel.order_by {
            let target = match expr {
                Expr::Position(p) => match usize::try_from(*p)
                    .ok()
                    .and_then(|p| p.checked_sub(1))
                    .filter(|p| *p < items.len())
                {
                    Some(i) => Target::Item(i),
                    None => return Err(unknown_column(&p.to_string(), "order clause")),
                },
                Expr::Column(c) => {
                    let lower = c.name.to_lowercase();
                    let by_alias = c.table.is_empty() && c.schema.is_empty();
                    let item = by_alias
                        .then(|| {
                            items.iter().position(|i| {
                                !i.alias.is_empty() && i.alias.to_lowercase() == lower
                            })
                        })
                        .flatten();
                    match item {
                        Some(i) => Target::Item(i),
                        None => Target::Column(self.resolve_column(c, "order clause")?),
                    }
                }
                _ => {
                    return Err(unsupported(
                        "ORDER BY items other than columns or positions",
                    ));
                }
            };
            order_targets.push((target, *desc));
        }

        let range = self.handle_range()?;
        let limit = self.limit()?;
        // Go's prepare describes the statement through the normal planner; the fast path only
        // applies to executions.
        let fast = if self.cx.describe_only {
            None
        } else {
            self.fast_point_get()
        };

        // Columns read from storage, in order of first use.
        let mut scan_columns: Vec<usize> = Vec::new();

        let source_kind = match range {
            // Go's logical optimizer turns LIMIT 0 into TableDual: nothing is read. (Pushing a
            // Limit 0 down would also crash TiKV, which asserts a non-empty batch.)
            _ if limit.is_some_and(|l| l.count == 0) => SourceKind::Dual,
            None => SourceKind::Dual,
            Some((lo, hi)) if lo == hi => SourceKind::Point(lo),
            Some((lo, hi)) => SourceKind::Range(lo, hi),
        };

        let mut output = Vec::new();
        let mut output_types = Vec::new();
        let mut sort = Vec::new();
        let mut root_agg = None;
        let mut source;

        if aggregated {
            // Functions computed by the (root) aggregation, in Go's order: the select-list
            // aggregates, then first_row of every plain column used in the output or ORDER BY.
            let mut funcs: Vec<(AggKind, usize)> = Vec::new();
            let mut func_of_item = Vec::new();
            let count_col = self.count_star_column();
            for it in &items {
                let f = match it.kind {
                    ItemKind::Agg(name, arg) => {
                        let col = match arg {
                            Some(c) => c,
                            None => count_col.clone()?,
                        };
                        let kind = match name {
                            AggName::Count => AggKind::Count,
                            AggName::Sum => AggKind::Sum,
                            AggName::Min => AggKind::Min,
                            AggName::Max => AggKind::Max,
                        };
                        (kind, col)
                    }
                    ItemKind::Col(col) => (AggKind::FirstRow, col),
                };
                func_of_item.push(push_func(&mut funcs, f));
            }
            let mut sort_funcs = Vec::new();
            for (target, desc) in &order_targets {
                let fi = match target {
                    Target::Item(i) => func_of_item[*i],
                    Target::Column(col) => {
                        if let Some(i) = items.iter().position(|it| it.kind == ItemKind::Col(*col))
                        {
                            func_of_item[i]
                        } else if group_cols.contains(col) {
                            push_func(&mut funcs, (AggKind::FirstRow, *col))
                        } else {
                            return Err(unsupported(
                                "ORDER BY a column that is neither selected nor grouped by",
                            ));
                        }
                    }
                };
                sort_funcs.push((fi, *desc));
            }
            // Types.
            let mut root_funcs = Vec::new();
            for &(kind, col) in &funcs {
                let ft = &self.column(col).field_type;
                check_agg_arg(kind, ft, &self.column(col).name.o)?;
                check_scannable(self.column(col))?;
                let ret = agg_ret_type(kind, ft);
                root_funcs.push(RootAggFunc {
                    kind,
                    arg: scan_pos(&mut scan_columns, col),
                    arg_type: ft.clone(),
                    ret_type: ret,
                });
            }
            let group_scan: Vec<usize> = group_cols
                .iter()
                .map(|&c| scan_pos(&mut scan_columns, c))
                .collect();
            for &c in &group_cols {
                check_scannable(self.column(c))?;
            }
            let group_types: Vec<FieldType> = group_cols
                .iter()
                .map(|&c| self.column(c).field_type.clone())
                .collect();
            source = match source_kind {
                SourceKind::Dual => Source::Dual,
                SourceKind::Point(h) => Source::PointGet { handle: h },
                SourceKind::Range(lo, hi) => Source::TableReader {
                    lo,
                    hi,
                    desc: false,
                    keep_order: false,
                    limit: None,
                    agg: Some(PushedAgg {
                        funcs: root_funcs
                            .iter()
                            .map(|f| to_pushed(f.kind, f.arg))
                            .collect(),
                        group_by: group_scan.clone(),
                    }),
                },
            };
            let agg = if matches!(source, Source::TableReader { .. }) {
                let n = root_funcs.len();
                RootAgg {
                    mode: AggMode::Final,
                    funcs: root_funcs
                        .iter()
                        .enumerate()
                        .map(|(i, f)| RootAggFunc {
                            kind: f.kind,
                            arg: i,
                            // TiKV's partial result has the function's result type.
                            arg_type: f.ret_type.clone(),
                            ret_type: f.ret_type.clone(),
                        })
                        .collect(),
                    group_by: (n..n + group_cols.len()).collect(),
                    group_types,
                }
            } else {
                RootAgg {
                    mode: AggMode::Complete,
                    funcs: root_funcs,
                    group_by: group_scan,
                    group_types,
                }
            };
            for (fi, desc) in sort_funcs {
                sort.push(SortKey {
                    col: fi,
                    desc,
                    ft: agg.funcs[fi].ret_type.clone(),
                });
            }
            for &fi in &func_of_item {
                output.push(fi);
                output_types.push(agg.funcs[fi].ret_type.clone());
            }
            root_agg = Some(agg);
        } else {
            for it in &items {
                let ItemKind::Col(col) = it.kind else {
                    unreachable!("aggregates imply `aggregated`")
                };
                check_scannable(self.column(col))?;
                output.push(scan_pos(&mut scan_columns, col));
                output_types.push(self.column(col).field_type.clone());
            }
            for (target, desc) in &order_targets {
                let col = match target {
                    Target::Item(i) => match items[*i].kind {
                        ItemKind::Col(c) => c,
                        ItemKind::Agg(..) => unreachable!(),
                    },
                    Target::Column(c) => *c,
                };
                check_scannable(self.column(col))?;
                sort.push(SortKey {
                    col: scan_pos(&mut scan_columns, col),
                    desc: *desc,
                    ft: self.column(col).field_type.clone(),
                });
            }
            source = match source_kind {
                SourceKind::Dual => Source::Dual,
                SourceKind::Point(h) => Source::PointGet { handle: h },
                SourceKind::Range(lo, hi) => Source::TableReader {
                    lo,
                    hi,
                    desc: false,
                    keep_order: true,
                    limit: None,
                    agg: None,
                },
            };
            // Sort elimination: ordering by the handle alone is the scan order.
            let handle_pos = self
                .handle_column()
                .and_then(|h| scan_columns.iter().position(|&c| c == h));
            if sort.len() == 1 && Some(sort[0].col) == handle_pos {
                if let Source::TableReader { desc, .. } = &mut source {
                    *desc = sort[0].desc;
                }
                sort.clear();
            }
            if !matches!(source, Source::TableReader { .. }) {
                // At most one row.
                sort.clear();
            }
            if sort.is_empty()
                && let (Some(l), Source::TableReader { limit: pushed, .. }) = (limit, &mut source)
            {
                *pushed = Some(l.offset.saturating_add(l.count));
            }
        }
        if scan_columns.is_empty() {
            // Unreachable in practice: every plan reads at least one column.
            return Err(unsupported("a table read without columns"));
        }

        // --- Result metadata ---
        if let Some(is_dual) = fast {
            // Go's fast path names the table as written (not the stored name).
            for it in &mut items {
                it.names.table = self.fast_tbl_name.clone();
            }
            if is_dual {
                source = Source::Dual;
            }
        }
        let columns = items
            .iter()
            .zip(&output_types)
            .map(|(it, ft)| column_def(ft, &it.names))
            .collect();

        Ok(Plan {
            table: self.table.clone(),
            scan_columns,
            source,
            agg: root_agg,
            sort,
            limit,
            output,
            output_types,
            columns,
        })
    }

    /// The column `COUNT(*)` counts: the handle, or any NOT NULL column.
    fn count_star_column(&self) -> Result<usize, SqlError> {
        if let Some(h) = self.handle_column() {
            return Ok(h);
        }
        self.table
            .columns
            .iter()
            .position(|c| {
                c.is_public()
                    && c.field_type.is_not_null()
                    && !c.is_virtual_generated()
                    && check_scannable(c).is_ok()
            })
            .ok_or_else(|| unsupported("COUNT(*) on a table without a NOT NULL column"))
    }
}

#[derive(Clone, Copy)]
enum SourceKind {
    Dual,
    Point(i64),
    Range(i64, i64),
}

fn scan_pos(cols: &mut Vec<usize>, idx: usize) -> usize {
    if let Some(p) = cols.iter().position(|&c| c == idx) {
        p
    } else {
        cols.push(idx);
        cols.len() - 1
    }
}

fn push_func(funcs: &mut Vec<(AggKind, usize)>, f: (AggKind, usize)) -> usize {
    // First-row functions are shared per column; aggregates are kept per select item, as Go
    // does (duplicates are cheap and keep the layout simple).
    if f.0 == AggKind::FirstRow
        && let Some(i) = funcs.iter().position(|g| *g == f)
    {
        return i;
    }
    funcs.push(f);
    funcs.len() - 1
}

fn to_pushed(kind: AggKind, arg: usize) -> AggFunc {
    match kind {
        AggKind::Count => AggFunc::Count(arg),
        AggKind::Sum => AggFunc::Sum(arg),
        AggKind::Min => AggFunc::Min(arg),
        AggKind::Max => AggFunc::Max(arg),
        AggKind::FirstRow => AggFunc::FirstRow(arg),
    }
}

/// Result type of an aggregate (Go `baseFuncDesc.typeInfer4*`, via the distsql port).
pub fn agg_ret_type(kind: AggKind, arg: &FieldType) -> FieldType {
    to_pushed(kind, 0).ret_type(arg)
}

fn is_numeric(t: u8) -> bool {
    matches!(
        t,
        tp::TINY
            | tp::SHORT
            | tp::INT24
            | tp::LONG
            | tp::LONGLONG
            | tp::YEAR
            | tp::NEWDECIMAL
            | tp::FLOAT
            | tp::DOUBLE
    )
}

fn check_agg_arg(kind: AggKind, ft: &FieldType, col: &str) -> Result<(), SqlError> {
    if kind == AggKind::Sum && !is_numeric(ft.tp) {
        return Err(unsupported(format!(
            "SUM over the non-numeric column '{col}'"
        )));
    }
    Ok(())
}

/// Columns whose values this node can read and return exactly like Go TiDB.
fn check_scannable(col: &ColumnInfo) -> Result<(), SqlError> {
    if col.is_virtual_generated() {
        return Err(unsupported(format!(
            "reading the virtual generated column '{}'",
            col.name.o
        )));
    }
    let ok = matches!(
        col.field_type.tp,
        tp::TINY
            | tp::SHORT
            | tp::INT24
            | tp::LONG
            | tp::LONGLONG
            | tp::YEAR
            | tp::NEWDECIMAL
            | tp::FLOAT
            | tp::DOUBLE
            | tp::STRING
            | tp::VARCHAR
            | tp::VAR_STRING
            | tp::TINY_BLOB
            | tp::MEDIUM_BLOB
            | tp::LONG_BLOB
            | tp::BLOB
            | tp::DATE
            | tp::DATETIME
            | tp::DURATION
    );
    if !ok {
        return Err(unsupported(format!(
            "reading column '{}' of type {} (TIMESTAMP/ENUM/SET/BIT/JSON/... are not supported \
             yet)",
            col.name.o, col.field_type.tp
        )));
    }
    Ok(())
}

/// Value range of a signed integer column type.
fn int_type_range(ft: &FieldType) -> Option<(i128, i128)> {
    let bits = match ft.tp {
        tp::TINY => 8,
        tp::SHORT => 16,
        tp::INT24 => 24,
        tp::LONG => 32,
        tp::LONGLONG => 64,
        _ => return None,
    };
    if ft.is_unsigned() {
        return Some((0, (1i128 << bits) - 1));
    }
    Some((-(1i128 << (bits - 1)), (1i128 << (bits - 1)) - 1))
}

fn truncate_name(s: &mut String) {
    const MAX: usize = 256;
    if s.len() > MAX {
        let mut end = MAX;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
    }
}

fn flatten_and<'e>(e: &'e Expr, out: &mut Vec<&'e Expr>) {
    match e {
        Expr::And(l, r) => {
            flatten_and(l, out);
            flatten_and(r, out);
        }
        other => out.push(other),
    }
}

fn is_const(e: &Expr) -> bool {
    match e {
        Expr::Literal(_) | Expr::Param(_) => true,
        Expr::Neg(inner) => is_const(inner),
        _ => false,
    }
}

impl fmt::Display for Plan {
    /// An EXPLAIN-like rendering, for tests and logs.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let t = &self.table;
        let scan_names: Vec<&str> = self
            .scan_columns
            .iter()
            .map(|&i| t.columns[i].name.o.as_str())
            .collect();
        let layout_name = |i: usize| -> String {
            match &self.agg {
                Some(agg) => {
                    let func = &agg.funcs[i];
                    let arg = match agg.mode {
                        AggMode::Final => format!("#{}", func.arg),
                        AggMode::Complete => scan_names[func.arg].to_owned(),
                    };
                    format!("{}({arg})", func.kind.name())
                }
                None => scan_names[i].to_owned(),
            }
        };
        let mut lines: Vec<String> = Vec::new();
        let out: Vec<String> = self.output.iter().map(|&i| layout_name(i)).collect();
        lines.push(format!("Projection [{}]", out.join(", ")));
        if let Some(l) = self.limit {
            lines.push(format!("Limit offset:{} count:{}", l.offset, l.count));
        }
        if !self.sort.is_empty() {
            let keys: Vec<String> = self
                .sort
                .iter()
                .map(|k| {
                    format!(
                        "{}{}",
                        layout_name(k.col),
                        if k.desc { ":desc" } else { "" }
                    )
                })
                .collect();
            lines.push(format!("Sort [{}]", keys.join(", ")));
        }
        if let Some(agg) = &self.agg {
            let funcs: Vec<String> = (0..agg.funcs.len()).map(layout_name).collect();
            let groups: Vec<String> = agg
                .group_by
                .iter()
                .map(|&g| match agg.mode {
                    AggMode::Final => format!("#{g}"),
                    AggMode::Complete => scan_names[g].to_owned(),
                })
                .collect();
            lines.push(format!(
                "HashAgg({}) funcs:[{}] group_by:[{}]",
                match agg.mode {
                    AggMode::Final => "final",
                    AggMode::Complete => "complete",
                },
                funcs.join(", "),
                groups.join(", ")
            ));
        }
        match &self.source {
            Source::Dual => lines.push("TableDual".to_owned()),
            Source::PointGet { handle } => lines.push(format!(
                "PointGet {} handle:{handle} columns:[{}]",
                t.name.o,
                scan_names.join(", ")
            )),
            Source::TableReader {
                lo,
                hi,
                desc,
                keep_order,
                limit,
                agg,
            } => {
                lines.push(format!("TableReader keep_order:{keep_order}"));
                if let Some(n) = limit {
                    lines.push(format!("Limit(cop) {n}"));
                }
                if let Some(a) = agg {
                    let funcs: Vec<String> = a
                        .funcs
                        .iter()
                        .map(|f| {
                            let (name, arg) = match *f {
                                AggFunc::Count(c) => ("count", c),
                                AggFunc::Sum(c) => ("sum", c),
                                AggFunc::Max(c) => ("max", c),
                                AggFunc::Min(c) => ("min", c),
                                AggFunc::FirstRow(c) => ("firstrow", c),
                            };
                            format!("{name}({})", scan_names[arg])
                        })
                        .collect();
                    let groups: Vec<&str> = a.group_by.iter().map(|&g| scan_names[g]).collect();
                    lines.push(format!(
                        "HashAgg(cop partial) funcs:[{}] group_by:[{}]",
                        funcs.join(", "),
                        groups.join(", ")
                    ));
                }
                lines.push(format!(
                    "TableRangeScan {} range:[{lo},{hi}]{} columns:[{}]",
                    t.name.o,
                    if *desc { " desc" } else { "" },
                    scan_names.join(", ")
                ));
            }
        }
        for (i, line) in lines.iter().enumerate() {
            if i == 0 {
                writeln!(f, "{line}")?;
            } else {
                writeln!(f, "{}└─{line}", "  ".repeat(i - 1))?;
            }
        }
        Ok(())
    }
}
