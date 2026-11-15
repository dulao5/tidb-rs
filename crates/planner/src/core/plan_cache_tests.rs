//! Tests of the plan cache's miss path (`plan_cache.rs`), `GeneratePlanCacheStmtWithAST`
//! and `isPlanCacheable` (D13.1; the AST check has govec vectors, `tests/govec_plancache.rs`).

use std::sync::Arc;

use tidb_base::Error;
use tidb_base::blocked::blocked_by;
use tidb_parser::model::CIStr;
use tidb_parser::{ParseOptions, parse};
use tidb_sessionctx::variable::session::{GlobalVarAccessor, SessionVars};
use tidb_types::datum::{Datum, new_int_datum, new_string_datum};

use crate::core::plan_cache::get_plan_from_plan_cache;
use crate::core::plan_cache::plan_cache_preprocess;
use crate::core::plan_cache_utils::{
    InfoSchemaLookup, PlanCacheStmt, generate_plan_cache_stmt_with_ast,
};
use crate::core::plan_cacheable_checker::is_plan_cacheable;
use crate::physicalop::BasePhysicalPlan;
use crate::{PhysicalPlan, PhysicalPlanRef, Schema, physical_plan_boilerplate};

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

fn vars() -> SessionVars {
    let mut v = SessionVars::new(Box::new(Acc));
    v.current_db = "test".to_owned();
    // The defaults of the system variables (a session sets them from the sysvar table).
    v.enable_prepared_plan_cache = true;
    v.enable_plan_cache_for_param_limit = true;
    v.enable_plan_cache_for_subquery = true;
    v.plan_cache_max_plan_size = 2 * 1024 * 1024;
    v
}

/// An InfoSchema without tables, at a version.
struct Is(i64);

impl InfoSchemaLookup for Is {
    fn table_by_name(
        &self,
        schema: &CIStr,
        table: &CIStr,
    ) -> Result<Arc<tidb_model::TableInfo>, Error> {
        Err(tidb_base::errors::new(format!(
            "no table {}.{}",
            schema.o, table.o
        )))
    }
    fn table_by_id(&self, _: i64) -> Result<Option<Arc<tidb_model::TableInfo>>, Error> {
        Ok(None)
    }
    fn schema_name_by_id(&self, _: i64) -> Option<CIStr> {
        None
    }
    fn schema_meta_version(&self) -> i64 {
        self.0
    }
    fn snapshot(&self) -> Arc<dyn InfoSchemaLookup> {
        Arc::new(Is(self.0))
    }
}

#[derive(Clone)]
struct Dual {
    base: BasePhysicalPlan,
    schema: Schema,
}

impl PhysicalPlan for Dual {
    physical_plan_boilerplate!(TableDual);
    fn schema(&self) -> &Schema {
        &self.schema
    }
    fn set_schema(&mut self, schema: Schema) {
        self.schema = schema;
    }
}

fn dual() -> PhysicalPlanRef {
    Arc::new(Dual {
        base: BasePhysicalPlan::default(),
        schema: Schema::default(),
    })
}

/// A prepared `sql` as `GeneratePlanCacheStmtWithAST` would build it (the plan builder
/// returns no plan yet, so the fields are set here).
fn stmt(sql: &str, params: Vec<usize>, version: i64) -> PlanCacheStmt {
    let mut s = PlanCacheStmt::default();
    s.prepared_ast.stmt = Box::new(
        parse(sql, &ParseOptions::default())
            .unwrap()
            .stmts
            .remove(0),
    );
    s.prepared_ast.stmt_type = "Select".to_owned();
    s.params = params;
    s.schema_version = version;
    s.stmt_text = sql.to_owned();
    s.stmt_cacheable = true;
    s
}

fn warnings(v: &SessionVars) -> Vec<String> {
    v.stmt_ctx()
        .get_warnings()
        .iter()
        .map(|w| String::from_utf8_lossy(&w.err.error()).into_owned())
        .collect()
}

#[test]
fn prepare_stops_at_the_plan_builder_after_the_cacheability_warning() {
    let mut v = vars();
    let sql = "select @a, ?";
    let st = parse(sql, &ParseOptions::default())
        .unwrap()
        .stmts
        .remove(0);
    let e = generate_plan_cache_stmt_with_ast(&mut v, true, sql, st, &Is(1))
        .err()
        .unwrap();
    assert_eq!(blocked_by(&e), Some("D09.1"));
    assert_eq!(
        warnings(&v),
        ["skip prepared plan-cache: query has user-defined variables is un-cacheable"]
    );
    // Plan cache off: Go records the reason without a warning.
    let mut v = vars();
    v.enable_prepared_plan_cache = false;
    let st = parse(sql, &ParseOptions::default())
        .unwrap()
        .stmts
        .remove(0);
    let e = generate_plan_cache_stmt_with_ast(&mut v, true, sql, st, &Is(1))
        .err()
        .unwrap();
    assert_eq!(blocked_by(&e), Some("D09.1"));
    assert!(warnings(&v).is_empty());
}

#[test]
fn execute_checks_the_parameter_count_and_sets_the_values_in_order() {
    let mut v = vars();
    let mut s = stmt("select ?, ?", vec![7, 10], 1);
    let e = plan_cache_preprocess(&mut v, false, &Is(1), &mut s, &[new_int_datum(1)]).unwrap_err();
    assert_eq!(e.code(), 8112, "{e}"); // ErrWrongParamCount
    plan_cache_preprocess(
        &mut v,
        false,
        &Is(1),
        &mut s,
        &[new_int_datum(1), new_string_datum("a")],
    )
    .unwrap();
    let vals = v.state.plan_cache_params.all_param_values();
    assert_eq!(vals.len(), 2);
    assert_eq!(vals[0].get_int64(), 1);
    assert_eq!(vals[1].get_string(), b"a");
    assert_eq!(v.stmt_ctx().stmt_type, "Select");
}

/// A schema change between PREPARE and EXECUTE: the statement is preprocessed again (Go's
/// `planCachePreprocess` step 4), the cached point get executor is dropped and the version
/// updated; a preprocess error becomes `ErrSchemaChanged`.
#[test]
fn execute_after_a_schema_change_preprocesses_again() {
    let mut v = vars();
    let mut s = stmt("select ?", vec![7], 1);
    s.point_get.executor = Some(Box::new(1u8));
    let ctx_before = Arc::clone(&s.resolve_ctx);
    plan_cache_preprocess(&mut v, false, &Is(2), &mut s, &[Datum::default()]).unwrap();
    assert_eq!(s.schema_version, 2);
    assert!(s.point_get.executor.is_none());
    assert!(!Arc::ptr_eq(&ctx_before, &s.resolve_ctx));
    // Same version: nothing is redone.
    s.point_get.executor = Some(Box::new(1u8));
    plan_cache_preprocess(&mut v, false, &Is(2), &mut s, &[Datum::default()]).unwrap();
    assert!(s.point_get.executor.is_some());

    let mut s = stmt("select * from (select 1) a, (select 2) a", vec![], 1);
    let e = plan_cache_preprocess(&mut v, false, &Is(2), &mut s, &[]).unwrap_err();
    assert_eq!(e.code(), 8113, "{e}");
    assert_eq!(
        String::from_utf8_lossy(&e.get_msg()),
        "Schema change caused error: [planner:1066]Not unique table/alias: 'a'"
    );
}

/// The miss path: the plan cache is used for the statement (`UseCache`), the optimizer is
/// called with `InPreparedPlanBuilding`, `isPlanCacheable` decides, and nothing is cached
/// (`FoundInPlanCache` stays false).
#[test]
fn miss_path_checks_the_new_plan() {
    let mut v = vars();
    let mut s = stmt("select ?", vec![7], 1);
    let mut calls = 0;
    let (p, _) = get_plan_from_plan_cache(
        &mut v,
        false,
        &Is(1),
        &mut s,
        &[new_int_datum(1)],
        &mut |v, _, _| {
            calls += 1;
            assert!(v.stmt_ctx().in_prepared_plan_building);
            Ok((dual(), Vec::new()))
        },
    )
    .unwrap();
    assert_eq!(calls, 1);
    assert_eq!(p.kind(), crate::PhysicalKind::TableDual);
    assert!(!v.stmt_ctx().in_prepared_plan_building);
    assert!(!v.found_in_plan_cache);
    // A table dual with parameters is not cacheable (Go's reason, as the warning).
    assert!(!v.stmt_ctx().plan_cache_tracker.use_cache());
    assert_eq!(
        warnings(&v),
        ["skip prepared plan-cache: get a TableDual plan"]
    );

    // The reason recorded at PREPARE is reported at every EXECUTE.
    let mut v = vars();
    let mut s = stmt("select 1", vec![], 1);
    s.stmt_cacheable = false;
    s.uncacheable_reason = "query has 'version' is un-cacheable".to_owned();
    get_plan_from_plan_cache(&mut v, false, &Is(1), &mut s, &[], &mut |_, _, _| {
        Ok((dual(), Vec::new()))
    })
    .unwrap();
    assert!(!v.stmt_ctx().plan_cache_tracker.use_cache());
    assert_eq!(
        warnings(&v),
        ["skip prepared plan-cache: query has 'version' is un-cacheable"]
    );

    // The optimizer's error is returned (production: OptimizeAstNode is a boundary stub).
    let mut v = vars();
    let mut s = stmt("select 1", vec![], 1);
    let e = get_plan_from_plan_cache(
        &mut v,
        false,
        &Is(1),
        &mut s,
        &[],
        &mut crate::optimize::optimize_ast_node,
    )
    .err()
    .unwrap();
    assert_eq!(blocked_by(&e), Some("D13.3"));
}

#[test]
fn is_plan_cacheable_switches_and_size() {
    let mut v = vars();
    let p = dual();
    assert_eq!(
        is_plan_cacheable(&v, &p, 0, 0, false).unwrap(),
        (true, String::new())
    );
    v.enable_plan_cache_for_param_limit = false;
    assert_eq!(
        is_plan_cacheable(&v, &p, 0, 1, false).unwrap().1,
        "the switch 'tidb_enable_plan_cache_for_param_limit' is off"
    );
    v.enable_plan_cache_for_subquery = false;
    assert_eq!(
        is_plan_cacheable(&v, &p, 0, 0, true).unwrap().1,
        "the switch 'tidb_enable_plan_cache_for_subquery' is off"
    );
    v.plan_cache_max_plan_size = 1;
    assert_eq!(
        is_plan_cacheable(&v, &p, 0, 0, false).unwrap().1,
        "plan is too large(decided by the variable @@tidb_plan_cache_max_plan_size)"
    );
    v.plan_cache_max_plan_size = 0;
    assert_eq!(
        is_plan_cacheable(&v, &p, 1, 0, false).unwrap().1,
        "get a TableDual plan"
    );
}
