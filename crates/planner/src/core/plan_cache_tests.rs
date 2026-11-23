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
fn prepare_builds_the_plan_after_the_cacheability_warning() {
    let mut v = vars();
    let sql = "select @a, ?";
    let st = parse(sql, &ParseOptions::default())
        .unwrap()
        .stmts
        .remove(0);
    // The plan builder runs (with the statement's planning context) and stops at the user
    // variable `@a` (its builtin function family is not ported).
    let e = generate_plan_cache_stmt_with_ast(
        &mut test_parts().plan_ctx(&mut v),
        true,
        sql,
        st,
        &Is(1),
        None,
    )
    .err()
    .unwrap();
    assert_eq!(blocked_by(&e), Some("D08.9"));
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
    let e = generate_plan_cache_stmt_with_ast(
        &mut test_parts().plan_ctx(&mut v),
        true,
        sql,
        st,
        &Is(1),
        None,
    )
    .err()
    .unwrap();
    assert_eq!(blocked_by(&e), Some("D08.9"));
    assert!(warnings(&v).is_empty());

    // PlanBuilder.Build is called again: a statement it builds gives PREPARE the plan's
    // schema and output names (the column definitions of the prepare response).
    let mut v = vars();
    let sql = "select ?, 1 as b";
    let st = parse(sql, &ParseOptions::default())
        .unwrap()
        .stmts
        .remove(0);
    let (ps, p, param_count) = generate_plan_cache_stmt_with_ast(
        &mut test_parts().plan_ctx(&mut v),
        true,
        sql,
        st,
        &Is(1),
        None,
    )
    .unwrap();
    assert_eq!(param_count, 1);
    assert_eq!(ps.params.len(), 1);
    let p = p.expect("the built plan");
    assert_eq!(p.schema.len(), 2);
    let names: Vec<String> = p
        .output_names
        .iter()
        .map(|n| n.col_name.o.clone())
        .collect();
    assert_eq!(names, ["?", "b"]);
}

#[test]
fn execute_checks_the_parameter_count_and_sets_the_values_in_order() {
    let mut v = vars();
    let mut s = stmt("select ?, ?", vec![7, 10], 1);
    let e = plan_cache_preprocess(&mut v, false, &Is(1), None, &mut s, &[new_int_datum(1)])
        .unwrap_err();
    assert_eq!(e.code(), 8112, "{e}"); // ErrWrongParamCount
    plan_cache_preprocess(
        &mut v,
        false,
        &Is(1),
        None,
        &mut s,
        &[new_int_datum(1), new_string_datum("a")],
    )
    .unwrap();
    let vals = v.state.plan_cache_params.lock().all_param_values().to_vec();
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
    plan_cache_preprocess(&mut v, false, &Is(2), None, &mut s, &[Datum::default()]).unwrap();
    assert_eq!(s.schema_version, 2);
    assert!(s.point_get.executor.is_none());
    assert!(!Arc::ptr_eq(&ctx_before, &s.resolve_ctx));
    // Same version: nothing is redone.
    s.point_get.executor = Some(Box::new(1u8));
    plan_cache_preprocess(&mut v, false, &Is(2), None, &mut s, &[Datum::default()]).unwrap();
    assert!(s.point_get.executor.is_some());

    let mut s = stmt("select * from (select 1) a, (select 2) a", vec![], 1);
    let e = plan_cache_preprocess(&mut v, false, &Is(2), None, &mut s, &[]).unwrap_err();
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
        None,
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
    get_plan_from_plan_cache(&mut v, false, &Is(1), None, &mut s, &[], &mut |_, _, _| {
        Ok((dual(), Vec::new()))
    })
    .unwrap();
    assert!(!v.stmt_ctx().plan_cache_tracker.use_cache());
    assert_eq!(
        warnings(&v),
        ["skip prepared plan-cache: query has 'version' is un-cacheable"]
    );

    // The optimizer plans `select 1` through the whole DoOptimize (FindBestTask, D10.2).
    let mut v = vars();
    let mut s = stmt("select 1", vec![], 1);
    get_plan_from_plan_cache(
        &mut v,
        false,
        &Is(1),
        None,
        &mut s,
        &[],
        &mut |vars, node, is| crate::optimize::optimize_ast_node(&test_parts(), vars, node, is),
    )
    .unwrap();
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

// ---------------------------------------------------------------- metadata lock

/// An InfoSchema with one table `test.t` (ID 10), at a version.
struct OneTable(i64, Arc<tidb_model::TableInfo>);

impl InfoSchemaLookup for OneTable {
    fn table_by_name(&self, _: &CIStr, table: &CIStr) -> Result<Arc<tidb_model::TableInfo>, Error> {
        if table.l == self.1.name.l {
            return Ok(Arc::clone(&self.1));
        }
        Err(tidb_base::errors::new("no table"))
    }
    fn table_by_id(&self, id: i64) -> Result<Option<Arc<tidb_model::TableInfo>>, Error> {
        Ok((id == self.1.id).then(|| Arc::clone(&self.1)))
    }
    fn schema_name_by_id(&self, _: i64) -> Option<CIStr> {
        None
    }
    fn schema_meta_version(&self) -> i64 {
        self.0
    }
    fn snapshot(&self) -> Arc<dyn InfoSchemaLookup> {
        Arc::new(OneTable(self.0, Arc::clone(&self.1)))
    }
}

fn mdl_table(revision: u64) -> tidb_model::TableInfo {
    let col = |id: i64, name: &str| {
        Arc::new(tidb_model::ColumnInfo {
            id,
            name: CIStr::new(name),
            state: tidb_model::StatePublic,
            ..Default::default()
        })
    };
    tidb_model::TableInfo {
        id: 10,
        name: CIStr::new("t"),
        state: tidb_model::StatePublic,
        revision,
        columns: Some(vec![col(1, "a"), col(2, "b")]),
        indices: Some(vec![Arc::new(tidb_model::IndexInfo {
            id: 1,
            name: CIStr::new("ia"),
            state: tidb_model::StatePublic,
            ..Default::default()
        })]),
        ..Default::default()
    }
}

fn mdl_vars(read_only: bool, in_txn: bool) -> SessionVars {
    let mut v = vars();
    v.state.txn_ctx.enable_mdl = true;
    v.stmt_ctx_mut().is_read_only = read_only;
    v.set_in_txn(in_txn);
    v
}

fn mdl_map(v: &SessionVars) -> Vec<(i64, i64)> {
    let mut m: Vec<_> = v
        .state
        .txn_ctx
        .related_table_for_mdl()
        .map(|m| m.0.lock().iter().map(|(k, v)| (*k, *v)).collect())
        .unwrap_or_default();
    m.sort();
    m
}

/// Go's skip conditions, in Go's order: MDL off, InfoSchema version 0, `tidb_snapshot`,
/// stale read, local temporary table, autocommit read-only statement. The statement's
/// related tables are recorded in every case (Go's `defer`), nothing is registered.
#[test]
fn metadata_lock_skips_as_go() {
    use crate::core::preprocess::try_lock_mdl_and_update_schema_if_necessary as lock;
    let db = CIStr::new("test");
    let t = Arc::new(mdl_table(1));
    let is = OneTable(5, Arc::clone(&t));
    type Setup = fn(&mut SessionVars, &mut tidb_model::TableInfo) -> i64;
    let cases: Vec<(&str, Setup)> = vec![
        ("MDL off", |v, _| {
            v.state.txn_ctx.enable_mdl = false;
            5
        }),
        ("version 0", |_, _| 0),
        ("tidb_snapshot", |v, _| {
            v.state.snapshot_infoschema = true;
            5
        }),
        ("stale read", |v, _| {
            v.state.txn_ctx.is_staleness = true;
            5
        }),
        ("local temporary table", |_, t| {
            t.temp_table_type = tidb_model::TempTableLocal;
            5
        }),
        ("autocommit read-only", |v, _| {
            v.stmt_ctx_mut().is_read_only = true;
            5
        }),
    ];
    for (name, setup) in cases {
        let mut v = mdl_vars(false, false);
        let mut ti = mdl_table(1);
        let ver = setup(&mut v, &mut ti);
        let ti = Arc::new(ti);
        let r = lock(
            &mut v,
            &db,
            Arc::clone(&ti),
            &OneTable(ver, Arc::clone(&ti)),
            None,
        )
        .unwrap();
        assert!(Arc::ptr_eq(&r, &ti), "{name}");
        assert_eq!(mdl_map(&v), vec![], "{name}");
        assert!(v.stmt_ctx().related_table_ids.contains(&10), "{name}");
    }
    // A global temporary table (Go: skipLock, SessionExtendedInfoSchema.MdlTables).
    let mut v = mdl_vars(false, false);
    let mut g = mdl_table(1);
    g.temp_table_type = tidb_model::TempTableGlobal;
    let e = lock(&mut v, &db, Arc::new(g), &is, Some(&is)).unwrap_err();
    assert_eq!(blocked_by(&e), Some("D06.3"));
    assert_eq!(mdl_map(&v), vec![]);
}

/// A statement that is not an autocommit read (PREPARE's fresh statement context, a
/// statement of an explicit transaction) registers the table at the domain's version; a
/// table already registered is not looked up again. Without the session's domain the
/// statement stops at a boundary stub and the 0 written first is deleted (Go's `defer`).
#[test]
fn metadata_lock_registers_at_the_domain_version() {
    use crate::core::preprocess::try_lock_mdl_and_update_schema_if_necessary as lock;
    let db = CIStr::new("test");
    let t = Arc::new(mdl_table(1));
    let is = OneTable(5, Arc::clone(&t));
    for (read_only, in_txn) in [(false, false), (true, true), (false, true)] {
        let mut v = mdl_vars(read_only, in_txn);
        let r = lock(&mut v, &db, Arc::clone(&t), &is, Some(&is)).unwrap();
        assert!(Arc::ptr_eq(&r, &t));
        assert_eq!(mdl_map(&v), vec![(10, 5)]);
        // Registered already: kept as it is, even if the domain moved on.
        let newer = OneTable(6, Arc::new(mdl_table(2)));
        lock(&mut v, &db, Arc::clone(&t), &is, Some(&newer)).unwrap();
        assert_eq!(mdl_map(&v), vec![(10, 5)]);
    }
    // No domain: blocked, and the placeholder 0 is removed again.
    let mut v = mdl_vars(false, false);
    let e = lock(&mut v, &db, Arc::clone(&t), &is, None).unwrap_err();
    assert_eq!(blocked_by(&e), Some("D06.5"));
    assert_eq!(mdl_map(&v), vec![]);
    // The domain has a newer definition of the table than the transaction's InfoSchema:
    // the session-extended InfoSchema that would carry it is not ported (blocked).
    let mut n = mdl_table(1);
    n.update_ts = 9;
    let newer = OneTable(6, Arc::new(n));
    let e = lock(&mut v, &db, Arc::clone(&t), &is, Some(&newer)).unwrap_err();
    assert_eq!(blocked_by(&e), Some("D06.3"));
    assert_eq!(mdl_map(&v), vec![]);
    // A table that is not public: ErrTableNotExists with the database and table names.
    let mut np = mdl_table(1);
    np.state = tidb_model::StateWriteOnly;
    let np = Arc::new(np);
    let mut v = mdl_vars(false, false);
    let d = OneTable(5, Arc::clone(&np));
    let e = lock(&mut v, &db, Arc::clone(&np), &d, Some(&d)).unwrap_err();
    assert_eq!(e.code(), 1146, "{e}");
    assert_eq!(e.to_string(), "[schema:1146]Table 'test.t' doesn't exist");
    assert_eq!(mdl_map(&v), vec![]);
    assert!(!v.stmt_ctx().related_table_ids.contains(&10));
}

/// The table-change check: an index that became public after the transaction's InfoSchema
/// is hidden in a copy (the shared definition keeps its state), a changed public column ID
/// is 8028 with Go's text.
#[test]
fn metadata_lock_table_change_check() {
    use crate::core::preprocess::check_table_change;
    let old = mdl_table(1);
    let mut new = mdl_table(2);
    new.indices
        .as_mut()
        .unwrap()
        .push(Arc::new(tidb_model::IndexInfo {
            id: 2,
            name: CIStr::new("ib"),
            state: tidb_model::StatePublic,
            ..Default::default()
        }));
    let new = Arc::new(new);
    let r = check_table_change(&old, Arc::clone(&new)).unwrap();
    assert!(!Arc::ptr_eq(&r, &new));
    let states: Vec<_> = r
        .indices
        .as_ref()
        .unwrap()
        .iter()
        .map(|i| i.state)
        .collect();
    assert_eq!(
        states,
        vec![
            tidb_model::StatePublic,
            tidb_model::StateWriteReorganization
        ]
    );
    assert_eq!(
        new.indices.as_ref().unwrap()[1].state,
        tidb_model::StatePublic
    );
    // Nothing new: the same table.
    let same = Arc::new(mdl_table(2));
    assert!(Arc::ptr_eq(
        &check_table_change(&old, Arc::clone(&same)).unwrap(),
        &same
    ));
    // A public column with another ID.
    let mut changed = mdl_table(2);
    Arc::make_mut(&mut changed.columns.as_mut().unwrap()[1]).id = 3;
    let e = check_table_change(&old, Arc::new(changed)).unwrap_err();
    assert_eq!(e.code(), 8028);
    assert_eq!(e.to_string(), "[domain:8028]public column b has changed");
}

/// `GetPreparedStmt`: the statement the node holds, else the one named in the session, else
/// Go's `ErrStmtNotFound` (8111); a value that is not a `PlanCacheStmt` is an error.
#[test]
fn get_prepared_stmt_from_the_node_or_by_name() {
    use crate::core::plan_cache_utils::get_prepared_stmt;
    use tidb_parser::ast::ExecuteStmt;
    let mut v = vars();
    let held = Arc::new(parking_lot::Mutex::new(stmt("select 1", vec![], 1)));
    let node = ExecuteStmt {
        prep_stmt: Some(Arc::clone(&held) as _),
        ..Default::default()
    };
    assert!(Arc::ptr_eq(&get_prepared_stmt(&node, &v).unwrap(), &held));

    let named = Arc::new(parking_lot::Mutex::new(stmt("select 2", vec![], 1)));
    v.add_prepared_stmt(3, Arc::clone(&named) as _).unwrap();
    v.state.prepared_stmt_name_to_id.insert("s".to_owned(), 3);
    let node = ExecuteStmt {
        name: "s".to_owned(),
        ..Default::default()
    };
    assert!(Arc::ptr_eq(&get_prepared_stmt(&node, &v).unwrap(), &named));

    let node = ExecuteStmt {
        name: "nope".to_owned(),
        ..Default::default()
    };
    assert_eq!(get_prepared_stmt(&node, &v).err().unwrap().code(), 8111);
    assert_eq!(
        get_prepared_stmt(&ExecuteStmt::default(), &v)
            .err()
            .unwrap()
            .code(),
        8111
    );
    let node = ExecuteStmt {
        prep_stmt: Some(Arc::new(())),
        ..Default::default()
    };
    assert!(get_prepared_stmt(&node, &v).is_err());
    v.remove_prepared_stmt(3);
}

/// `OptimizeExecStmt`: the `Execute` plan takes the binary arguments and the statement, then
/// the plan cache's miss path runs (the statement's parameters are set) and ends in the
/// statement's `Optimize`, a boundary stub; `EXECUTE ... USING @var` needs the expression
/// rewriter first.
#[test]
fn optimize_exec_stmt_reaches_the_statement_optimizer() {
    use crate::core::planbuilder::new_plan_builder;
    use crate::optimize::optimize_exec_stmt;
    use tidb_parser::ast::{ExecuteStmt, Stmt};
    let mut v = vars();
    let prep = Arc::new(parking_lot::Mutex::new(stmt("select ? + 1", vec![7], 1)));
    let node = ExecuteStmt {
        binary_args: Some(Arc::new(vec![new_int_datum(5)])),
        prep_stmt: Some(Arc::clone(&prep) as _),
        prep_stmt_id: 1,
        ..Default::default()
    };
    let exec = new_plan_builder(&[]).build_execute(&v, &node).unwrap();
    assert_eq!(exec.params.len(), 1);
    assert_eq!(exec.params[0].get_int64(), 5);
    assert!(Arc::ptr_eq(&exec.prep_stmt, &prep));
    assert!(exec.plan.is_none());

    // The session's expression context reads the parameter values (here a static context
    // that holds them, as the session's sessionexpr context will).
    optimize_exec_stmt(
        &mut test_parts_with_params(&[new_int_datum(5)]).plan_ctx(&mut v),
        &node,
        &Is(1),
        None,
    )
    .unwrap();
    // The miss path ran: the parameter values are in the session, and the statement was
    // being planned (the flag is cleared again).
    assert_eq!(v.state.plan_cache_params.lock().all_param_values().len(), 1);
    assert!(!v.stmt_ctx().in_prepared_plan_building);

    // A wrong parameter count is Go's error, before the optimizer.
    let node = ExecuteStmt {
        binary_args: Some(Arc::new(Vec::<Datum>::new())),
        prep_stmt: Some(Arc::clone(&prep) as _),
        ..Default::default()
    };
    let e = optimize_exec_stmt(&mut test_parts().plan_ctx(&mut v), &node, &Is(1), None)
        .err()
        .unwrap();
    assert_eq!(e.code(), 8112, "{e}");

    // EXECUTE s USING @a: the variables go through the expression rewriter.
    let Stmt::Execute(node) = parse("EXECUTE s USING @a", &ParseOptions::default())
        .unwrap()
        .stmts
        .remove(0)
    else {
        panic!("not an EXECUTE");
    };
    assert_eq!(node.name, "s");
    assert_eq!(node.using_vars.len(), 1);
    let e = optimize_exec_stmt(&mut test_parts().plan_ctx(&mut v), &node, &Is(1), None)
        .err()
        .unwrap();
    assert_eq!(blocked_by(&e), Some("D09.2"), "{e}");
}

/// The session's parts of the planning context, for the tests that plan: a static
/// expression context, no store client, no dirty table.
fn test_parts() -> crate::planctx::SessionPlanCtx {
    crate::planctx::SessionPlanCtx {
        // A statement's plan cache tracker (not enabled), as the session's planning context.
        expr_ctx: tidb_sessionctx::exprstatic::new_expr_context(vec![
            tidb_sessionctx::exprstatic::with_plan_cache_tracker(Arc::new(
                tidb_base::contextutil::new_plan_cache_tracker(None),
            )),
        ]),
        client: None,
        has_dirty_content: Some(Arc::new(|_| false)),
        advise_txn_warmup: None,
        domain_info_schema: None,
    }
}

/// [`test_parts`] whose expression context holds the EXECUTE parameter values.
fn test_parts_with_params(params: &[Datum]) -> crate::planctx::SessionPlanCtx {
    use tidb_sessionctx::exprstatic as es;
    let ev = es::new_eval_context(vec![es::with_param_list(params)]);
    crate::planctx::SessionPlanCtx {
        expr_ctx: es::new_expr_context(vec![es::with_eval_ctx(ev)]),
        ..test_parts()
    }
}
