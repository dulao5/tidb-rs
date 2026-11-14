//! Test helpers: a session, an expression context, and the plans Go's `PlanBuilder` builds
//! for a few statements (same operators, same plan-id order), over the planner test suite's
//! table `test.t(a int primary key, b int, c int, d int, e int)`.
#![allow(dead_code)]

use std::sync::Arc;

use tidb_base::Error;
use tidb_base::mysql::{NotNullFlag, PriKeyFlag, TypeLonglong};
use tidb_expression::{Column, Expr, ExprRef, Schema, new_one, new_schema};
use tidb_model::{ColumnInfo, TableInfo};
use tidb_parser::ast::funcs as ast;
use tidb_parser::model::CIStr;
use tidb_planner_pilot::aggregation::{AggFuncDesc, new_agg_func_desc};
use tidb_planner_pilot::logicalop::{DataSource, LogicalAggregation, LogicalProjection};
use tidb_planner_pilot::util::HandleCols;
use tidb_planner_pilot::{LogicalPlan, PlanCtx, PlanId};
use tidb_sessionctx::exprctx::ExprContextRef;
use tidb_sessionctx::exprstatic as es;
use tidb_sessionctx::variable::session::{GlobalVarAccessor, SessionVars};
use tidb_types::etc::FieldType;
use tidb_types::field_type::new_field_type;

struct Acc;

impl GlobalVarAccessor for Acc {
    fn get_global_sys_var(&self, _: &str) -> Result<String, Error> {
        Ok(String::new())
    }
    fn set_global_sys_var_only(&mut self, _: &str, _: &str, _: bool) -> Result<(), Error> {
        Ok(())
    }
    fn get_tidb_table_value(&self, _: &str) -> Result<String, Error> {
        Ok(String::new())
    }
    fn set_tidb_table_value(&mut self, _: &str, _: &str, _: &str) -> Result<(), Error> {
        Ok(())
    }
}

/// A fresh session (plan ids and column ids start at 1).
pub fn session() -> SessionVars {
    SessionVars::new(Box::new(Acc))
}

/// A static expression context.
pub fn expr_ctx() -> ExprContextRef {
    es::new_expr_context(Vec::new())
}

pub fn int_type(not_null: bool) -> FieldType {
    let mut tp = new_field_type(TypeLonglong);
    if not_null {
        tp.add_flag(NotNullFlag);
    }
    tp
}

pub fn col_expr(c: &Arc<Column>) -> ExprRef {
    Expr::Column((**c).clone()).into_ref()
}

pub fn one() -> ExprRef {
    Expr::Constant(new_one()).into_ref()
}

pub fn func(ctx: &PlanCtx<'_>, name: &str, args: Vec<ExprRef>) -> ExprRef {
    tidb_expression::scalar_function::new_function(&*ctx.expr_ctx, name, int_type(false), args)
        .expect("new_function")
}

/// The output column the builder allocates for a non-column select field.
pub fn new_col(ctx: &PlanCtx<'_>, tp: FieldType) -> Arc<Column> {
    Arc::new(Column {
        unique_id: ctx.vars.alloc_plan_column_id(),
        ret_type: tp,
        ..Column::default()
    })
}

/// `buildDataSource` for `test.t`: columns a (int primary key, the int handle), b, c, d, e
/// (`b_nullable` controls whether b, c, d, e are nullable).
pub fn data_source(ctx: &mut PlanCtx<'_>) -> PlanId {
    data_source_with(ctx, &["a", "b", "c", "d", "e"])
}

pub fn data_source_with(ctx: &mut PlanCtx<'_>, names: &[&str]) -> PlanId {
    let mut cols = Vec::new();
    let mut infos = Vec::new();
    for (i, n) in names.iter().enumerate() {
        let mut tp = int_type(i == 0);
        if i == 0 {
            tp.add_flag(PriKeyFlag);
        }
        let info = ColumnInfo {
            id: i as i64 + 1,
            name: CIStr::new(n),
            offset: i as isize,
            field_type: tp.clone(),
            ..ColumnInfo::default()
        };
        cols.push(Arc::new(Column {
            unique_id: ctx.vars.alloc_plan_column_id(),
            id: info.id,
            ret_type: tp,
            orig_name: format!("test.t.{n}"),
            ..Column::default()
        }));
        infos.push(info);
    }
    let table_info = Arc::new(TableInfo {
        id: 100,
        name: CIStr::new("t"),
        pk_is_handle: true,
        columns: Some(infos.iter().cloned().map(Arc::new).collect()),
        ..TableInfo::default()
    });
    let handle = HandleCols::Int(cols[0].clone());
    let mut ds = DataSource::init(
        CIStr::new("test"),
        table_info,
        infos,
        new_schema(cols),
        ctx,
        1,
    );
    ds.handle_cols = Some(handle.clone());
    ds.un_mutable_handle_cols = Some(handle);
    ctx.arena.alloc(Box::new(ds))
}

pub fn schema_of(ctx: &PlanCtx<'_>, p: PlanId) -> Schema {
    ctx.arena.get(p).schema(&ctx.arena).clone()
}

/// `buildAggregation`: the given aggregate functions, then `firstrow(col)` for every column
/// of the child (output column = a clone of the child column), and the group-by items.
pub fn aggregation(
    ctx: &mut PlanCtx<'_>,
    child: PlanId,
    funcs: Vec<(&str, Vec<ExprRef>)>,
    group_by: Vec<ExprRef>,
) -> (PlanId, Vec<Arc<Column>>) {
    let mut agg_funcs: Vec<AggFuncDesc> = Vec::new();
    let mut out = Vec::new();
    let mut schema_cols = Vec::new();
    for (name, args) in funcs {
        let desc = new_agg_func_desc(&*ctx.expr_ctx, name, args, false).unwrap();
        let col = new_col(ctx, desc.ret_tp.clone());
        out.push(col.clone());
        schema_cols.push(col);
        agg_funcs.push(desc);
    }
    for col in schema_of(ctx, child).columns {
        let desc = new_agg_func_desc(
            &*ctx.expr_ctx,
            ast::AggFuncFirstRow,
            vec![col_expr(&col)],
            false,
        )
        .unwrap();
        schema_cols.push(Arc::new((*col).clone()));
        agg_funcs.push(desc);
    }
    let mut agg = LogicalAggregation::init(agg_funcs, group_by, new_schema(schema_cols), ctx, 1);
    agg.set_children(vec![child]);
    (ctx.arena.alloc(Box::new(agg)), out)
}

/// `buildProjection` with the given (expression, output column) pairs.
pub fn projection(
    ctx: &mut PlanCtx<'_>,
    child: PlanId,
    fields: Vec<(ExprRef, Arc<Column>)>,
) -> PlanId {
    let (exprs, cols): (Vec<_>, Vec<_>) = fields.into_iter().unzip();
    let mut proj = LogicalProjection::init(exprs, new_schema(cols), ctx, 1);
    proj.set_children(vec![child]);
    ctx.arena.alloc(Box::new(proj))
}

/// Column strings of a schema, as Go's `checkDataSourceCols` prints them.
pub fn col_strings(s: &Schema) -> Vec<String> {
    s.columns
        .iter()
        .map(|c| String::from_utf8_lossy(&c.to_string()).into_owned())
        .collect()
}
