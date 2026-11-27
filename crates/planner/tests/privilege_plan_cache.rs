//! Plan cache privilege re-check and user-scoped cache key tests
//! (`specs/D04.1-privilege/spec.md` §5.2 item 8, `D13.1`, `A3` decision 9, `R16`, `R29`–`R32`):
//! - `PREPARE` rejects unauthorized statements and records non-empty `PlanCacheStmt.visit_infos`
//!   (including zero-column-reference queries such as `SELECT 1 FROM test.t WHERE a = ?`).
//! - `EXECUTE` populates `StmtCtx.tables` from `PlanCacheStmt.visit_infos`.
//! - Every plan cache hit re-checks live privileges (`Handle::replace`) and live `active_roles`
//!   (`SET ROLE`) inside `adjust_cached_plan -> check_prepared_priv` (never caching boolean
//!   decisions, resetting `found_in_plan_cache = false` on denial).
//! - Instance plan cache OFF still rejects revoked privileges on `EXECUTE`.
//! - `new_plan_cache_key` includes length-prefixed `(user_name, host_name)` (`A3` decision 9,
//!   `R16`), isolating users in the shared instance plan cache and preventing concatenation
//!   collisions such as `'a127.0.0.'@'%'` vs `'a'@'127.0.0.%'`.

use std::sync::Arc;

use tidb_base::Error;
use tidb_base::gostd::time::utc;
use tidb_base::mysql::{InsertPriv, NotNullFlag, PrivilegeType, SQLMode, SelectPriv, TypeLong};
use tidb_kv::checker::RequestTypeSupportedChecker;
use tidb_kv::kv::{Client, ClientSendOption, Request, Response};
use tidb_model::{ColumnInfo, StatePublic, TableInfo};
use tidb_parser::ast::ExecuteStmt;
use tidb_parser::auth::{RoleIdentity, UserIdentity};
use tidb_parser::model::CIStr;
use tidb_parser::types::FieldType;
use tidb_parser::{ParseOptions, parse};
use tidb_planner::core::plan_cache_instance::{InstancePlanCacheImpl, new_instance_plan_cache};
use tidb_planner::core::plan_cache_utils::{
    InfoSchemaLookup, PlanCacheKeyEnv, PlanCacheStmt, generate_plan_cache_stmt_with_ast,
    new_plan_cache_key,
};
use tidb_planner::optimize::optimize_exec_stmt;
use tidb_planner::planctx::SessionPlanCtx;
use tidb_privilege::{
    ColumnsPrivRecord, Handle, MySQLPrivilege, PrivilegeManagerRef, TablesPrivRecord,
    UserPrivileges, UserRecord,
};
use tidb_sessionctx::exprstatic as es;
use tidb_sessionctx::stmtctx::new_stmt_ctx_with_time_zone;
use tidb_sessionctx::variable::session::{GlobalVarAccessor, SessionVars};
use tidb_types::datum::{Datum, new_int_datum};

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

struct MockClient;

impl Client for MockClient {
    fn send(
        &self,
        _: &tidb_kv::Context,
        _: &Request,
        _: Option<tidb_kv::kv::OptionValue>,
        _: &ClientSendOption,
    ) -> Box<dyn Response> {
        unreachable!("MockClient::send")
    }
    fn is_request_type_supported(&self, req_type: i64, sub_type: i64) -> bool {
        RequestTypeSupportedChecker.is_request_type_supported(req_type, sub_type)
    }
}

struct MockIs(Vec<Arc<TableInfo>>);

impl InfoSchemaLookup for MockIs {
    fn table_by_name(&self, schema: &CIStr, table: &CIStr) -> Result<Arc<TableInfo>, Error> {
        if schema.l == "test"
            && let Some(t) = self.0.iter().find(|t| t.name.l == table.l)
        {
            return Ok(Arc::clone(t));
        }
        Err(
            tidb_infoschema::error::ErrTableNotExists.fast_gen_by_args(vec![
                tidb_base::gofmt::Arg::from(schema.o.clone()),
                tidb_base::gofmt::Arg::from(table.o.clone()),
            ]),
        )
    }
    fn table_by_id(&self, id: i64) -> Result<Option<Arc<TableInfo>>, Error> {
        Ok(self.0.iter().find(|t| t.id == id).cloned())
    }
    fn schema_name_by_id(&self, id: i64) -> Option<CIStr> {
        (id == 1).then(|| CIStr::new("test"))
    }
    fn schema_meta_version(&self) -> i64 {
        1
    }
    fn snapshot(&self) -> Arc<dyn InfoSchemaLookup> {
        Arc::new(MockIs(self.0.clone()))
    }
}

fn int_col(id: i64, offset: isize, name: &str) -> Arc<ColumnInfo> {
    let mut ft = FieldType::new(TypeLong);
    ft.set_flen(11);
    ft.set_flag(NotNullFlag);
    Arc::new(ColumnInfo {
        id,
        name: CIStr::new(name),
        offset,
        field_type: ft,
        state: StatePublic,
        ..Default::default()
    })
}

fn non_pk_table(id: i64, name: &str) -> Arc<TableInfo> {
    Arc::new(TableInfo {
        id,
        dbid: 1,
        name: CIStr::new(name),
        columns: Some(vec![int_col(1, 0, "a"), int_col(2, 1, "b")]),
        state: StatePublic,
        pk_is_handle: false,
        is_common_handle: false,
        revision: 1,
        ..Default::default()
    })
}

fn make_vars(user: &str, host: &str, active_roles: Vec<RoleIdentity>) -> SessionVars {
    let mut vars = SessionVars::new(Box::new(Acc));
    vars.current_db = "test".to_owned();
    vars.sql_mode = SQLMode::default().0;
    vars.time_zone = Some(utc());
    vars.enable_prepared_plan_cache = true;
    vars.user = Some(UserIdentity {
        username: user.to_owned(),
        hostname: host.to_owned(),
        ..Default::default()
    });
    vars.active_roles = active_roles;
    vars
}

fn reset_stmt_ctx(vars: &mut SessionVars) {
    vars.set_stmt_ctx(Arc::new(new_stmt_ctx_with_time_zone(utc())));
}

fn parts_with(
    vars: &SessionVars,
    params: &[Datum],
    cache: Arc<InstancePlanCacheImpl>,
    pm: PrivilegeManagerRef,
) -> SessionPlanCtx {
    let ev = es::new_eval_context(vec![es::with_param_list(params)]);
    let expr_ctx = es::new_expr_context(vec![
        es::with_eval_ctx(ev),
        es::with_plan_cache_tracker(Arc::clone(&vars.stmt_ctx().plan_cache_tracker)),
    ]);
    SessionPlanCtx {
        expr_ctx,
        client: Some(Arc::new(MockClient)),
        has_dirty_content: Some(Arc::new(|_| false)),
        advise_txn_warmup: None,
        domain_info_schema: None,
        instance_plan_cache: Some(cache),
        privilege_manager: Some(pm),
    }
}

fn user_pm(handle: &Arc<Handle>, user: &str, host: &str) -> PrivilegeManagerRef {
    Arc::new(UserPrivileges::new(user, host, Arc::clone(handle)))
}

fn priv_with_table_select(user: &str) -> MySQLPrivilege {
    let mut p = MySQLPrivilege::new();
    p.add_user(UserRecord::new("%", user, PrivilegeType(0)));
    p.add_table_priv(TablesPrivRecord::new(user, "%", "test", "t", SelectPriv));
    p
}

fn priv_with_col_a_select(user: &str) -> MySQLPrivilege {
    let mut p = MySQLPrivilege::new();
    p.add_user(UserRecord::new("%", user, PrivilegeType(0)));
    p.add_column_priv(ColumnsPrivRecord::new(
        user, "%", "test", "t", "a", SelectPriv,
    ));
    p
}

fn priv_with_role_select(user: &str, role: &str) -> MySQLPrivilege {
    let mut p = MySQLPrivilege::new();
    p.add_user(UserRecord::new("%", user, PrivilegeType(0)));
    p.add_user(UserRecord::new("%", role, PrivilegeType(0)));
    p.add_table_priv(TablesPrivRecord::new(role, "%", "test", "t", SelectPriv));
    p.add_role_edge(role, "%", user, "%");
    p
}

fn prepare_sql(
    vars: &mut SessionVars,
    is: &MockIs,
    cache: &Arc<InstancePlanCacheImpl>,
    pm: &PrivilegeManagerRef,
    sql: &str,
) -> Result<PlanCacheStmt, Error> {
    reset_stmt_ctx(vars);
    let out = parse(sql, &ParseOptions::default()).unwrap();
    let markers = out.param_markers;
    let stmt = out.stmts.into_iter().next().unwrap();
    let parts = parts_with(vars, &[], Arc::clone(cache), Arc::clone(pm));
    let mut ctx = parts.plan_ctx(vars);
    let (ps, _, _) =
        generate_plan_cache_stmt_with_ast(&mut ctx, true, sql, stmt, markers, is, None)?;
    Ok(ps)
}

fn execute_prepared(
    vars: &mut SessionVars,
    is: &MockIs,
    cache: &Arc<InstancePlanCacheImpl>,
    pm: &PrivilegeManagerRef,
    ps: &Arc<parking_lot::Mutex<PlanCacheStmt>>,
    params: Vec<Datum>,
) -> Result<(), Error> {
    reset_stmt_ctx(vars);
    let parts = parts_with(vars, &params, Arc::clone(cache), Arc::clone(pm));
    let exec_ast = ExecuteStmt {
        binary_args: Some(Arc::new(params)),
        prep_stmt: Some(Arc::clone(ps) as _),
        prep_stmt_id: 1,
        ..Default::default()
    };
    let mut ctx = parts.plan_ctx(vars);
    optimize_exec_stmt(&mut ctx, &exec_ast, is, None)?;
    Ok(())
}

#[test]
fn prepare_and_execute_plan_cache_hit_rechecks_live_privileges_and_roles() {
    let is = MockIs(vec![non_pk_table(10, "t")]);
    let cache = Arc::new(new_instance_plan_cache(1024 * 1024, 4 * 1024 * 1024));

    // 1. PREPARE rejects unauthorized users immediately (1142 for table-denied, 1143 for column-denied).
    {
        let mut p = MySQLPrivilege::new();
        p.add_user(UserRecord::new("%", "u_none", PrivilegeType(0)));
        p.add_user(UserRecord::new("%", "u_ins", PrivilegeType(0)));
        p.add_table_priv(TablesPrivRecord::new("u_ins", "%", "test", "t", InsertPriv));
        p.add_user(UserRecord::new("%", "u_cola", PrivilegeType(0)));
        p.add_column_priv(ColumnsPrivRecord::new(
            "u_cola", "%", "test", "t", "a", SelectPriv,
        ));
        let handle = Arc::new(Handle::new(p));

        for user in ["u_none", "u_ins"] {
            let mut vars = make_vars(user, "%", vec![]);
            let pm = user_pm(&handle, user, "%");
            let err = prepare_sql(
                &mut vars,
                &is,
                &cache,
                &pm,
                "SELECT 1 FROM test.t WHERE a = ?",
            )
            .err()
            .expect("PREPARE must reject zero-SELECT user");
            assert_eq!(err.code(), 1142, "user={user} err={err}");
        }

        let mut vars = make_vars("u_cola", "%", vec![]);
        let pm = user_pm(&handle, "u_cola", "%");
        for sql in [
            "SELECT b FROM test.t WHERE a = ?",
            "SELECT _tidb_rowid FROM test.t WHERE a = ?",
        ] {
            let err = prepare_sql(&mut vars, &is, &cache, &pm, sql)
                .err()
                .expect("PREPARE must reject unauthorized column / _tidb_rowid");
            assert_eq!(err.code(), 1143, "sql={sql} err={err}");
        }
    }

    // 2. Live REVOKE blocks cached EXECUTE immediately for `SELECT 1 FROM test.t WHERE a = ?`
    //    (instance plan cache ON: 1st miss -> 2nd hit -> REVOKE -> 3rd denied with 1142).
    {
        let handle = Arc::new(Handle::new(priv_with_table_select("u_tbl")));
        let pm = user_pm(&handle, "u_tbl", "%");
        let mut vars = make_vars("u_tbl", "%", vec![]);

        let ps = prepare_sql(
            &mut vars,
            &is,
            &cache,
            &pm,
            "SELECT 1 FROM test.t WHERE a = ?",
        )
        .expect("u_tbl PREPARE");
        assert!(
            !ps.visit_infos.is_empty(),
            "PlanCacheStmt.visit_infos must retain table-read R29 VisitInfo"
        );
        let ps = Arc::new(parking_lot::Mutex::new(ps));

        // 1st EXECUTE: miss -> caches plan & populates StmtCtx.tables.
        execute_prepared(&mut vars, &is, &cache, &pm, &ps, vec![new_int_datum(1)]).unwrap();
        assert!(!vars.found_in_plan_cache);
        assert!(
            vars.stmt_ctx()
                .tables
                .iter()
                .any(|t| t.db == "test" && t.table == "t"),
            "EXECUTE must populate StmtCtx.tables from PlanCacheStmt.visit_infos"
        );

        // 2nd EXECUTE: plan cache HIT.
        execute_prepared(&mut vars, &is, &cache, &pm, &ps, vec![new_int_datum(2)]).unwrap();
        assert!(vars.found_in_plan_cache, "expected plan cache hit");

        // Live REVOKE: drop all privileges for u_tbl.
        let mut revoked = MySQLPrivilege::new();
        revoked.add_user(UserRecord::new("%", "u_tbl", PrivilegeType(0)));
        handle.replace(revoked);

        // 3rd EXECUTE: cached entry exists, but adjust_cached_plan -> check_prepared_priv rejects!
        let err = execute_prepared(&mut vars, &is, &cache, &pm, &ps, vec![new_int_datum(3)])
            .expect_err("revoked user must be denied on plan cache hit");
        assert_eq!(err.code(), 1142, "expected 1142 after REVOKE, got {err}");
        assert!(
            !vars.found_in_plan_cache,
            "denied cache hit must leave found_in_plan_cache == false"
        );

        // Instance plan cache OFF: miss path also rejects revoked user with 1142.
        vars.enable_prepared_plan_cache = false;
        let err = execute_prepared(&mut vars, &is, &cache, &pm, &ps, vec![new_int_datum(4)])
            .expect_err("revoked user must be denied with plan cache OFF");
        assert_eq!(err.code(), 1142, "expected 1142 with cache OFF, got {err}");
    }

    // 3. Downgrading from full-table SELECT to SELECT(a) rejects cached `SELECT b` and
    //    `SELECT _tidb_rowid` (R32) with 1143 on cache hit.
    for (sql, expected_col) in [
        ("SELECT b FROM test.t WHERE a = ?", "b"),
        ("SELECT _tidb_rowid FROM test.t WHERE a = ?", "_tidb_rowid"),
    ] {
        let handle = Arc::new(Handle::new(priv_with_table_select("u_downgrade")));
        let pm = user_pm(&handle, "u_downgrade", "%");
        let mut vars = make_vars("u_downgrade", "%", vec![]);

        let ps = Arc::new(parking_lot::Mutex::new(
            prepare_sql(&mut vars, &is, &cache, &pm, sql).unwrap(),
        ));
        execute_prepared(&mut vars, &is, &cache, &pm, &ps, vec![new_int_datum(1)]).unwrap();
        execute_prepared(&mut vars, &is, &cache, &pm, &ps, vec![new_int_datum(2)]).unwrap();
        assert!(vars.found_in_plan_cache, "expected cache hit for {sql}");

        // Downgrade to SELECT(a) only.
        handle.replace(priv_with_col_a_select("u_downgrade"));

        let err = execute_prepared(&mut vars, &is, &cache, &pm, &ps, vec![new_int_datum(3)])
            .expect_err("downgraded user must be rejected on cache hit");
        assert_eq!(err.code(), 1143, "sql={sql} err={err}");
        let raw = err.error();
        let msg = String::from_utf8_lossy(&raw);
        assert!(
            msg.contains(&format!("for column '{expected_col}' in table 't'")),
            "sql={sql} msg={msg}"
        );
        assert!(!vars.found_in_plan_cache);
    }

    // 4. Live SET ROLE (`active_roles` change) re-checked on every cache hit.
    {
        let handle = Arc::new(Handle::new(priv_with_role_select("u_role", "r_read")));
        let pm = user_pm(&handle, "u_role", "%");
        let role = RoleIdentity {
            username: "r_read".to_owned(),
            hostname: "%".to_owned(),
        };
        let mut vars = make_vars("u_role", "%", vec![role.clone()]);

        let ps = Arc::new(parking_lot::Mutex::new(
            prepare_sql(
                &mut vars,
                &is,
                &cache,
                &pm,
                "SELECT 1 FROM test.t WHERE a = ?",
            )
            .unwrap(),
        ));
        // 1st miss, 2nd hit with active role.
        execute_prepared(&mut vars, &is, &cache, &pm, &ps, vec![new_int_datum(1)]).unwrap();
        execute_prepared(&mut vars, &is, &cache, &pm, &ps, vec![new_int_datum(2)]).unwrap();
        assert!(vars.found_in_plan_cache);

        // SET ROLE NONE -> denied immediately on cache hit.
        vars.active_roles.clear();
        let err = execute_prepared(&mut vars, &is, &cache, &pm, &ps, vec![new_int_datum(3)])
            .expect_err("SET ROLE NONE must deny cached EXECUTE");
        assert_eq!(err.code(), 1142, "err={err}");
        assert!(!vars.found_in_plan_cache);

        // SET ROLE r_read -> cache hit succeeds again.
        vars.active_roles.push(role);
        execute_prepared(&mut vars, &is, &cache, &pm, &ps, vec![new_int_datum(4)]).unwrap();
        assert!(vars.found_in_plan_cache);
    }
}

#[test]
fn plan_cache_key_isolates_users_and_avoids_user_host_collisions() {
    let is = MockIs(vec![non_pk_table(10, "t")]);
    let cache = Arc::new(new_instance_plan_cache(1024 * 1024, 4 * 1024 * 1024));
    let mut p = MySQLPrivilege::new();
    for (u, h) in [
        ("u1", "%"),
        ("u2", "%"),
        ("a127.0.0.", "%"),
        ("a", "127.0.0.%"),
    ] {
        p.add_user(UserRecord::new(h, u, PrivilegeType(0)));
        p.add_table_priv(TablesPrivRecord::new(u, h, "test", "t", SelectPriv));
    }
    let handle = Arc::new(Handle::new(p));

    let sql = "SELECT a FROM test.t WHERE a = ?";

    // Distinct users sharing one InstancePlanCacheImpl must not share cached plans.
    let mut v1 = make_vars("u1", "%", vec![]);
    let pm1 = user_pm(&handle, "u1", "%");
    let ps1 = Arc::new(parking_lot::Mutex::new(
        prepare_sql(&mut v1, &is, &cache, &pm1, sql).unwrap(),
    ));
    execute_prepared(&mut v1, &is, &cache, &pm1, &ps1, vec![new_int_datum(1)]).unwrap();
    assert!(!v1.found_in_plan_cache);
    execute_prepared(&mut v1, &is, &cache, &pm1, &ps1, vec![new_int_datum(2)]).unwrap();
    assert!(v1.found_in_plan_cache, "u1 should hit its own cache entry");

    let mut v2 = make_vars("u2", "%", vec![]);
    let pm2 = user_pm(&handle, "u2", "%");
    let ps2 = Arc::new(parking_lot::Mutex::new(
        prepare_sql(&mut v2, &is, &cache, &pm2, sql).unwrap(),
    ));
    execute_prepared(&mut v2, &is, &cache, &pm2, &ps2, vec![new_int_datum(1)]).unwrap();
    assert!(
        !v2.found_in_plan_cache,
        "u2 must NOT hit u1's cached plan (A3 decision 9)"
    );

    // R16 boundary: length-prefixed (user_name, host_name) prevents concatenation collisions
    // between ('a127.0.0.', '%') and ('a', '127.0.0.%').
    let mut va = make_vars("a127.0.0.", "%", vec![]);
    let pma = user_pm(&handle, "a127.0.0.", "%");
    let mut psa = prepare_sql(&mut va, &is, &cache, &pma, sql).unwrap();
    new_plan_cache_key(&mut va, &mut psa, &PlanCacheKeyEnv { domain_is: None }).unwrap();

    let mut vb = make_vars("a", "127.0.0.%", vec![]);
    let pmb = user_pm(&handle, "a", "127.0.0.%");
    let mut psb = prepare_sql(&mut vb, &is, &cache, &pmb, sql).unwrap();
    new_plan_cache_key(&mut vb, &mut psb, &PlanCacheKeyEnv { domain_is: None }).unwrap();

    assert_ne!(
        psa.cache_key, psb.cache_key,
        "length-prefixed cache key must distinguish ('a127.0.0.', '%') from ('a', '127.0.0.%') (R16)"
    );
}
