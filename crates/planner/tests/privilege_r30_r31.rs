//! End-to-end planner privilege enforcement tests for rulings R29, R30, and R31
//! (`specs/D04.1-privilege/spec.md` §3.2 matrix, §5.2, §5.3 item 1, §6.1 D1/D3/D4):
//! - R29: unauthorized queries without column references (`SELECT 1`, `COUNT(*)`, `SUM(1)`,
//!   PointGet overflow `IsTableDual`) are rejected with `1142` / `1143`.
//! - R30: users with zero `SELECT` privilege on a table receive `1142` (`for table 't'`)
//!   before `1143` (`for column 'a' in table 't'`), closing the `1143` vs `1142` existence oracle.
//! - R31: missing-table checks in `preprocess::table_by_name` check `SelectPriv` (`column = "*"`)
//!   for read statements so non-SELECT grantees (`u_db_ins` holding `INSERT ON test.*`) receive
//!   `1142` instead of `1146`, while `SELECT ON test.*` (`u_db_sel`) and `root` receive `1146`.

use std::sync::Arc;

use tidb_base::Error;
use tidb_base::gostd::time::utc;
use tidb_base::mysql::{
    InsertPriv, NotNullFlag, PriKeyFlag, PrivilegeType, SQLMode, SelectPriv, TypeLong, UpdatePriv,
};
use tidb_kv::checker::RequestTypeSupportedChecker;
use tidb_kv::kv::{Client, ClientSendOption, Request, Response};
use tidb_model::{ColumnInfo, IndexColumn, IndexInfo, StatePublic, TableInfo};
use tidb_parser::auth::UserIdentity;
use tidb_parser::model::CIStr;
use tidb_parser::types::FieldType;
use tidb_parser::{ParseOptions, parse};
use tidb_planner::core::plan_cache_utils::InfoSchemaLookup;
use tidb_planner::core::preprocess::{
    InfoSchemaHandle, PreprocessorReturn, preprocess, with_preprocessor_return,
    with_privilege_manager,
};
use tidb_planner::core::resolve::new_node_w;
use tidb_planner::optimize::optimize_ast_node;
use tidb_planner::planctx::SessionPlanCtx;
use tidb_privilege::{
    ColumnsPrivRecord, DbRecord, Handle, MySQLPrivilege, PrivilegeManagerRef, TablesPrivRecord,
    UserPrivileges, UserRecord,
};
use tidb_sessionctx::exprstatic as es;
use tidb_sessionctx::variable::session::{GlobalVarAccessor, SessionVars};

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

fn int_col(id: i64, offset: isize, name: &str, pk: bool) -> Arc<ColumnInfo> {
    let mut ft = FieldType::new(TypeLong);
    ft.set_flen(11);
    if pk {
        ft.set_flag(PriKeyFlag | NotNullFlag);
    }
    Arc::new(ColumnInfo {
        id,
        name: CIStr::new(name),
        offset,
        field_type: ft,
        state: StatePublic,
        ..Default::default()
    })
}

fn spec_tables() -> Arc<MockIs> {
    let t = Arc::new(TableInfo {
        id: 10,
        dbid: 1,
        name: CIStr::new("t"),
        columns: Some(vec![int_col(1, 0, "a", false), int_col(2, 1, "b", false)]),
        state: StatePublic,
        ..Default::default()
    });
    let t2 = Arc::new(TableInfo {
        id: 11,
        dbid: 1,
        name: CIStr::new("t2"),
        columns: Some(vec![int_col(1, 0, "x", false)]),
        state: StatePublic,
        ..Default::default()
    });
    let t_uk = Arc::new(TableInfo {
        id: 12,
        dbid: 1,
        name: CIStr::new("t_uk"),
        columns: Some(vec![
            int_col(1, 0, "id", true),
            int_col(2, 1, "a", false),
            int_col(3, 2, "b", false),
        ]),
        indices: Some(vec![Arc::new(IndexInfo {
            id: 1,
            name: CIStr::new("uk_a"),
            table: CIStr::new("t_uk"),
            columns: Some(vec![IndexColumn {
                name: CIStr::new("a"),
                offset: 1,
                length: -1,
                ..Default::default()
            }]),
            state: StatePublic,
            unique: true,
            ..Default::default()
        })]),
        pk_is_handle: true,
        state: StatePublic,
        ..Default::default()
    });
    Arc::new(MockIs(vec![t, t2, t_uk]))
}

/// Builds the exact spec §3.1 account matrix inside a single live `Handle`.
fn spec_privilege_handle() -> Arc<Handle> {
    let mut mp = MySQLPrivilege::new();
    // root: global SELECT
    mp.add_user(UserRecord::new("%", "root", SelectPriv));
    // u_none: zero grants
    mp.add_user(UserRecord::new("%", "u_none", PrivilegeType(0)));
    // u_ins: INSERT, UPDATE ON test.t (no SELECT)
    mp.add_user(UserRecord::new("%", "u_ins", PrivilegeType(0)));
    mp.add_table_priv(TablesPrivRecord::new(
        "u_ins",
        "%",
        "test",
        "t",
        PrivilegeType(InsertPriv.0 | UpdatePriv.0),
    ));
    // u_other: SELECT ON test.t2 (no grant on test.t)
    mp.add_user(UserRecord::new("%", "u_other", PrivilegeType(0)));
    mp.add_table_priv(TablesPrivRecord::new(
        "u_other", "%", "test", "t2", SelectPriv,
    ));
    // u_cola: SELECT(a) ON test.t
    mp.add_user(UserRecord::new("%", "u_cola", PrivilegeType(0)));
    mp.add_column_priv(ColumnsPrivRecord::new(
        "u_cola", "%", "test", "t", "a", SelectPriv,
    ));
    // u_colb: SELECT(b) ON test.t_uk
    mp.add_user(UserRecord::new("%", "u_colb", PrivilegeType(0)));
    mp.add_column_priv(ColumnsPrivRecord::new(
        "u_colb", "%", "test", "t_uk", "b", SelectPriv,
    ));
    // u_mixed: UPDATE(a) + SELECT(b) ON test.t (verifies MatchColumns "*" predicate, spec §5.2 item 1)
    mp.add_user(UserRecord::new("%", "u_mixed", PrivilegeType(0)));
    mp.add_column_priv(ColumnsPrivRecord::new(
        "u_mixed", "%", "test", "t", "a", UpdatePriv,
    ));
    mp.add_column_priv(ColumnsPrivRecord::new(
        "u_mixed", "%", "test", "t", "b", SelectPriv,
    ));
    // u_tbl: SELECT ON test.t
    mp.add_user(UserRecord::new("%", "u_tbl", PrivilegeType(0)));
    mp.add_table_priv(TablesPrivRecord::new("u_tbl", "%", "test", "t", SelectPriv));
    // u_db_ins: INSERT ON test.* (no SELECT)
    mp.add_user(UserRecord::new("%", "u_db_ins", PrivilegeType(0)));
    mp.add_db(DbRecord::new("u_db_ins", "%", "test", InsertPriv));
    // u_db_sel: SELECT ON test.*
    mp.add_user(UserRecord::new("%", "u_db_sel", PrivilegeType(0)));
    mp.add_db(DbRecord::new("u_db_sel", "%", "test", SelectPriv));
    Arc::new(Handle::new(mp))
}

fn plan_as_user(
    tables: &Arc<MockIs>,
    handle: &Arc<Handle>,
    username: Option<&str>,
    current_db: &str,
    sql: &str,
) -> Result<(), Error> {
    let out = parse(sql, &ParseOptions::default()).unwrap_or_else(|e| panic!("parse {sql}: {e}"));
    let mut stmt = out.stmts.into_iter().next().unwrap();
    let mut vars = SessionVars::new(Box::new(Acc));
    vars.current_db = current_db.to_owned();
    let pm: Option<PrivilegeManagerRef> = username.map(|u| {
        vars.user = Some(UserIdentity {
            username: u.to_owned(),
            hostname: "%".to_owned(),
            ..Default::default()
        });
        Arc::new(UserPrivileges::new(u, "%", Arc::clone(handle))) as PrivilegeManagerRef
    });

    let is_dyn: Arc<dyn InfoSchemaLookup> = Arc::clone(tables) as Arc<dyn InfoSchemaLookup>;
    let mut node_w = new_node_w(&mut stmt);
    node_w.set_text(sql);
    let mut opts = vec![with_preprocessor_return(PreprocessorReturn {
        info_schema: Some(InfoSchemaHandle(Arc::clone(&is_dyn))),
        ..Default::default()
    })];
    if let Some(pm) = &pm {
        opts.push(with_privilege_manager(Arc::clone(pm)));
    }
    let (_, res) = preprocess(&mut vars, &mut node_w, opts);
    res?;

    let ev = es::new_eval_context(vec![
        es::with_type_flags(tidb_types::context::DefaultStmtFlags),
        es::with_sql_mode(SQLMode(0)),
        es::with_location(Some(utc())),
        es::with_current_db(current_db.to_owned()),
    ]);
    let expr_ctx = es::new_expr_context(vec![
        es::with_eval_ctx(ev),
        es::with_charset("utf8mb4".to_owned(), "utf8mb4_bin".to_owned()),
        es::with_default_collation_for_utf8mb4("utf8mb4_bin".to_owned()),
    ]);
    let parts = SessionPlanCtx {
        expr_ctx,
        client: Some(Arc::new(MockClient)),
        has_dirty_content: Some(Arc::new(|_| false)),
        advise_txn_warmup: None,
        domain_info_schema: Some(Arc::clone(&is_dyn)),
        instance_plan_cache: None,
        privilege_manager: pm,
    };
    optimize_ast_node(&parts, &mut vars, &mut node_w, &*is_dyn)?;
    Ok(())
}

fn err_code(res: Result<(), Error>) -> Option<isize> {
    res.err().map(|e| e.code())
}

/// Asserts that `optimize_ast_node` passed privilege verification (either returning `Ok(())` or
/// advancing past `CheckPrivilege` to a downstream non-privilege optimizer boundary stub such as
/// `D09.13` `BuildKeySolver` for aggregation queries).
fn assert_privilege_passed(res: Result<(), Error>, who: &str, sql: &str) {
    if let Err(e) = res {
        let block = tidb_base::blocked::blocked_by(&e);
        assert!(
            block.is_some() && block != Some("D04.1"),
            "{who} should pass privilege checks on `{sql}`, got error: {e}"
        );
    }
}

/// Pins ruling R30 (and R29 enforcement) across the spec §3.2 account matrix:
/// - Users with zero SELECT on `test.t` (`u_none`, `u_ins`, `u_other`, `u_db_ins`) uniformly
///   receive `1142` (`for table 't'`) on `SELECT 1`, `SELECT COUNT(*)`, `SELECT SUM(1)`,
///   `SELECT a`, `SELECT b`, `SELECT *`, `TABLE test.t` — NEVER `OK` and NEVER `1143`.
/// - Users with partial-column `SELECT(a)` (`u_cola`) pass `"*"` (`SELECT 1`, `SELECT COUNT(*)`,
///   `SELECT a` -> `OK`), receive `1143` on unauthorized column `b`, and `1142` on `SELECT *` / `TABLE`.
/// - Users with `UPDATE(a) + SELECT(b)` (`u_mixed`) pass `"*"` and `SELECT b`, and fail with
///   `1143` on `SELECT a`.
/// - Secondary-unique-index PointGet `IsTableDual` overflow (`SELECT b FROM test.t_uk WHERE a = <overflow> AND a = 1`):
///   `u_none` gets `1142`, `u_colb` gets `1143` for column `a` (Go v8.5.8 wrongly returned `OK`).
#[test]
fn r30_table_denied_1142_precedes_column_denied_1143() {
    let tables = spec_tables();
    let handle = spec_privilege_handle();

    // Zero-SELECT accounts on test.t (db=None, qualified test.t): ALL queries return 1142.
    for user in ["u_none", "u_ins", "u_other", "u_db_ins"] {
        for sql in [
            "SELECT 1 FROM test.t",
            "SELECT 'x' FROM test.t LIMIT 1",
            "SELECT 1 FROM test.t WHERE 1=0",
            "SELECT 1 FROM test.t WHERE 1=1",
            "SELECT a FROM test.t",
            "SELECT b FROM test.t",
            "SELECT 1 FROM test.t WHERE b = 1",
            "SELECT * FROM test.t",
            "TABLE test.t",
            "SELECT count(*) FROM test.t",
            "SELECT COUNT(*) FROM test.t",
            "SELECT Count(*) FROM test.t",
            "SELECT SUM(1) FROM test.t",
            "SELECT MAX(1) FROM test.t",
            "SELECT 1 FROM (SELECT 1 FROM test.t) sub",
        ] {
            let err = plan_as_user(&tables, &handle, Some(user), "", sql)
                .expect_err(&format!("{user} must be denied on `{sql}`"));
            assert_eq!(err.code(), 1142, "{user} on `{sql}`: {err}");
            let raw = err.error();
            let msg = String::from_utf8_lossy(&raw);
            assert!(
                msg.contains(&format!("'{user}'@'%'")) && msg.contains("for table 't'"),
                "{user} on `{sql}`: unexpected 1142 message: {msg}"
            );
        }
    }

    // u_cola (SELECT(a) ON test.t): passes table-read "*", passes `a`, fails `b` with 1143, fails `*` with 1142.
    for ok_sql in [
        "SELECT 1 FROM test.t",
        "SELECT COUNT(*) FROM test.t",
        "SELECT a FROM test.t",
    ] {
        assert_privilege_passed(
            plan_as_user(&tables, &handle, Some("u_cola"), "", ok_sql),
            "u_cola",
            ok_sql,
        );
    }
    for col_b_sql in ["SELECT b FROM test.t", "SELECT 1 FROM test.t WHERE b = 1"] {
        let err = plan_as_user(&tables, &handle, Some("u_cola"), "", col_b_sql).unwrap_err();
        assert_eq!(err.code(), 1143, "u_cola on `{col_b_sql}`: {err}");
        let raw = err.error();
        let msg = String::from_utf8_lossy(&raw);
        assert!(
            msg.contains("for column 'b' in table 't'"),
            "u_cola on `{col_b_sql}`: {msg}"
        );
    }
    for wild_sql in ["SELECT * FROM test.t", "TABLE test.t"] {
        assert_eq!(
            err_code(plan_as_user(&tables, &handle, Some("u_cola"), "", wild_sql)),
            Some(1142),
            "u_cola on `{wild_sql}`"
        );
    }

    // u_mixed (UPDATE(a) + SELECT(b) ON test.t): "*" skips UPDATE(a) and matches SELECT(b).
    assert_eq!(
        err_code(plan_as_user(
            &tables,
            &handle,
            Some("u_mixed"),
            "",
            "SELECT 1 FROM test.t"
        )),
        None
    );
    assert_eq!(
        err_code(plan_as_user(
            &tables,
            &handle,
            Some("u_mixed"),
            "",
            "SELECT b FROM test.t"
        )),
        None
    );
    assert_eq!(
        err_code(plan_as_user(
            &tables,
            &handle,
            Some("u_mixed"),
            "",
            "SELECT a FROM test.t"
        )),
        Some(1143)
    );

    // u_tbl (SELECT ON test.t): all queries on test.t pass privilege checks.
    for sql in [
        "SELECT 1 FROM test.t",
        "SELECT a, b FROM test.t",
        "SELECT * FROM test.t",
        "SELECT COUNT(*) FROM test.t",
    ] {
        assert_privilege_passed(
            plan_as_user(&tables, &handle, Some("u_tbl"), "", sql),
            "u_tbl",
            sql,
        );
    }

    // PointGet secondary-unique-index overflow IsTableDual (Go v8.5.8 returned OK for u_colb!).
    let uk_overflow = "SELECT b FROM test.t_uk WHERE a = 999999999999999999999 AND a = 1";
    assert_eq!(
        err_code(plan_as_user(
            &tables,
            &handle,
            Some("u_none"),
            "",
            uk_overflow
        )),
        Some(1142)
    );
    assert_eq!(
        err_code(plan_as_user(
            &tables,
            &handle,
            Some("u_colb"),
            "",
            uk_overflow
        )),
        Some(1143)
    );
}

/// Pins ruling R31: missing-table check in `preprocess::table_by_name` uses statement-appropriate
/// `SelectPriv` (`column = "*"`) for read statements:
/// - `u_db_ins` (`INSERT ON test.*`, no `SELECT`) gets `1142` (Go v8.5.8 leaked `1146`).
/// - `u_none`, `u_ins`, `u_other`, `u_cola`, `u_tbl` all get `1142` on `test.nosuch`, identical
///   to `test.t` for zero-SELECT accounts (no `1142` vs `1143`/`1146` oracle).
/// - `u_db_sel` (`SELECT ON test.*`), `root` (global `SELECT`), and unbound `pm == None` get `1146`.
#[test]
fn r31_missing_table_1142_vs_1146_oracle_closed() {
    let tables = spec_tables();
    let handle = spec_privilege_handle();

    for user in ["u_none", "u_ins", "u_other", "u_cola", "u_tbl", "u_db_ins"] {
        for sql in [
            "SELECT 1 FROM test.nosuch",
            "SELECT a FROM test.nosuch",
            "SELECT COUNT(*) FROM test.nosuch",
            "SELECT * FROM test.nosuch",
        ] {
            let err = plan_as_user(&tables, &handle, Some(user), "", sql)
                .expect_err(&format!("{user} must fail on `{sql}`"));
            assert_eq!(err.code(), 1142, "{user} on `{sql}`: {err}");
            let raw = err.error();
            let msg = String::from_utf8_lossy(&raw);
            assert!(
                msg.contains(&format!("'{user}'@'%'")) && msg.contains("for table 'nosuch'"),
                "{user} on `{sql}`: unexpected 1142 message: {msg}"
            );
        }
    }

    for user in [Some("u_db_sel"), Some("root"), None] {
        let err = plan_as_user(&tables, &handle, user, "", "SELECT 1 FROM test.nosuch")
            .expect_err("missing table must return 1146");
        assert_eq!(err.code(), 1146, "user={user:?}: {err}");
    }
}
