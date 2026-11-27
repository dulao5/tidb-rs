//! End-to-end planner privilege enforcement tests for ruling R32
//! (`specs/D04.1-privilege/spec.md` §5.2 item 1, §6.1 D5 option A):
//! Reading the hidden `_tidb_rowid` column of a non-PK-handle table requires full-table
//! (or database/global) `SELECT` privilege (`column = ""`):
//! - Zero-SELECT users (`u_none`, `u_ins`, `u_other`, `u_db_ins`) fail at the table-read
//!   baseline with `1142` (`for table 't'`, R30).
//! - Partial-column `SELECT(a)` (`u_cola`) and `UPDATE(a) + SELECT(b)` (`u_mixed`) pass
//!   table-read `"*"` but fail on `_tidb_rowid`'s `column = ""` with `1143`
//!   (`for column '_tidb_rowid' in table 't'`, R32), across both regular SELECT and PointGet
//!   fast-path queries (`WHERE _tidb_rowid = 1`) as well as derived tables.
//! - Full-table (`u_tbl`), database (`u_db_sel`), and global (`root`) `SELECT` users succeed.

use std::sync::Arc;

use tidb_base::Error;
use tidb_base::gostd::time::utc;
use tidb_base::mysql::{InsertPriv, PrivilegeType, SQLMode, SelectPriv, TypeLong, UpdatePriv};
use tidb_kv::checker::RequestTypeSupportedChecker;
use tidb_kv::kv::{Client, ClientSendOption, Request, Response};
use tidb_model::{ColumnInfo, StatePublic, TableInfo};
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

fn int_col(id: i64, offset: isize, name: &str) -> Arc<ColumnInfo> {
    let mut ft = FieldType::new(TypeLong);
    ft.set_flen(11);
    Arc::new(ColumnInfo {
        id,
        name: CIStr::new(name),
        offset,
        field_type: ft,
        state: StatePublic,
        ..Default::default()
    })
}

fn non_pk_table() -> Arc<MockIs> {
    // test.t(a INT, b INT): !pk_is_handle && !is_common_handle -> has hidden _tidb_rowid
    let t = Arc::new(TableInfo {
        id: 10,
        dbid: 1,
        name: CIStr::new("t"),
        columns: Some(vec![int_col(1, 0, "a"), int_col(2, 1, "b")]),
        state: StatePublic,
        ..Default::default()
    });
    let t2 = Arc::new(TableInfo {
        id: 11,
        dbid: 1,
        name: CIStr::new("t2"),
        columns: Some(vec![int_col(1, 0, "x")]),
        state: StatePublic,
        ..Default::default()
    });
    Arc::new(MockIs(vec![t, t2]))
}

fn priv_handle() -> Arc<Handle> {
    let mut mp = MySQLPrivilege::new();
    mp.add_user(UserRecord::new("%", "root", SelectPriv));
    mp.add_user(UserRecord::new("%", "u_none", PrivilegeType(0)));
    mp.add_user(UserRecord::new("%", "u_ins", PrivilegeType(0)));
    mp.add_table_priv(TablesPrivRecord::new(
        "u_ins",
        "%",
        "test",
        "t",
        PrivilegeType(InsertPriv.0 | UpdatePriv.0),
    ));
    mp.add_user(UserRecord::new("%", "u_other", PrivilegeType(0)));
    mp.add_table_priv(TablesPrivRecord::new(
        "u_other", "%", "test", "t2", SelectPriv,
    ));
    mp.add_user(UserRecord::new("%", "u_cola", PrivilegeType(0)));
    mp.add_column_priv(ColumnsPrivRecord::new(
        "u_cola", "%", "test", "t", "a", SelectPriv,
    ));
    mp.add_user(UserRecord::new("%", "u_mixed", PrivilegeType(0)));
    mp.add_column_priv(ColumnsPrivRecord::new(
        "u_mixed", "%", "test", "t", "a", UpdatePriv,
    ));
    mp.add_column_priv(ColumnsPrivRecord::new(
        "u_mixed", "%", "test", "t", "b", SelectPriv,
    ));
    mp.add_user(UserRecord::new("%", "u_tbl", PrivilegeType(0)));
    mp.add_table_priv(TablesPrivRecord::new("u_tbl", "%", "test", "t", SelectPriv));
    mp.add_user(UserRecord::new("%", "u_db_sel", PrivilegeType(0)));
    mp.add_db(DbRecord::new("u_db_sel", "%", "test", SelectPriv));
    Arc::new(Handle::new(mp))
}

fn plan_as(tables: &Arc<MockIs>, handle: &Arc<Handle>, user: &str, sql: &str) -> Result<(), Error> {
    let out = parse(sql, &ParseOptions::default()).unwrap_or_else(|e| panic!("parse {sql}: {e}"));
    let mut stmt = out.stmts.into_iter().next().unwrap();
    let mut vars = SessionVars::new(Box::new(Acc));
    vars.user = Some(UserIdentity {
        username: user.to_owned(),
        hostname: "%".to_owned(),
        ..Default::default()
    });
    let pm: PrivilegeManagerRef = Arc::new(UserPrivileges::new(user, "%", Arc::clone(handle)));
    let is_dyn: Arc<dyn InfoSchemaLookup> = Arc::clone(tables) as Arc<dyn InfoSchemaLookup>;

    let mut node_w = new_node_w(&mut stmt);
    node_w.set_text(sql);
    let (_, res) = preprocess(
        &mut vars,
        &mut node_w,
        vec![
            with_preprocessor_return(PreprocessorReturn {
                info_schema: Some(InfoSchemaHandle(Arc::clone(&is_dyn))),
                ..Default::default()
            }),
            with_privilege_manager(Arc::clone(&pm)),
        ],
    );
    res?;

    let ev = es::new_eval_context(vec![
        es::with_type_flags(tidb_types::context::DefaultStmtFlags),
        es::with_sql_mode(SQLMode(0)),
        es::with_location(Some(utc())),
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
        privilege_manager: Some(pm),
    };
    optimize_ast_node(&parts, &mut vars, &mut node_w, &*is_dyn)?;
    Ok(())
}

/// Pins ruling R32 across regular SELECT, PointGet fast path, and derived tables.
#[test]
fn r32_tidb_rowid_requires_full_table_select() {
    let tables = non_pk_table();
    let handle = priv_handle();

    let rowid_queries = [
        "SELECT _tidb_rowid FROM test.t",
        "SELECT 1 FROM test.t WHERE _tidb_rowid = 1",
        "SELECT a FROM test.t WHERE _tidb_rowid = 1",
        "SELECT _tidb_rowid FROM test.t WHERE _tidb_rowid = 1",
        "SELECT _tidb_rowid FROM (SELECT _tidb_rowid FROM test.t) sub",
        "SELECT a FROM test.t ORDER BY _tidb_rowid",
    ];

    // Zero-SELECT users receive 1142 (R30 table-denied before column-denied).
    for user in ["u_none", "u_ins", "u_other"] {
        for sql in rowid_queries {
            let err = plan_as(&tables, &handle, user, sql)
                .expect_err(&format!("{user} must be denied on `{sql}`"));
            assert_eq!(err.code(), 1142, "{user} on `{sql}`: {err}");
            let raw = err.error();
            let msg = String::from_utf8_lossy(&raw);
            assert!(
                msg.contains("for table 't'"),
                "{user} on `{sql}`: unexpected message: {msg}"
            );
        }
    }

    // Partial-column users (`u_cola` SELECT(a), `u_mixed` UPDATE(a)+SELECT(b)) pass table-read
    // `"*"` but fail on `_tidb_rowid`'s full-table (`column = ""`) requirement with 1143.
    for user in ["u_cola", "u_mixed"] {
        for sql in [
            "SELECT _tidb_rowid FROM test.t",
            "SELECT 1 FROM test.t WHERE _tidb_rowid = 1",
            "SELECT _tidb_rowid FROM test.t WHERE _tidb_rowid = 1",
            "SELECT _tidb_rowid FROM (SELECT _tidb_rowid FROM test.t) sub",
        ] {
            let err = plan_as(&tables, &handle, user, sql)
                .expect_err(&format!("{user} must be denied on `{sql}`"));
            assert_eq!(err.code(), 1143, "{user} on `{sql}`: {err}");
            let raw = err.error();
            let msg = String::from_utf8_lossy(&raw);
            assert!(
                msg.contains("for column '_tidb_rowid' in table 't'"),
                "{user} on `{sql}`: unexpected message: {msg}"
            );
        }
    }
    // Specifically verify u_cola on `SELECT a FROM test.t WHERE _tidb_rowid = 1` (PointGet path!)
    // and `SELECT a FROM test.t ORDER BY _tidb_rowid`: both allow `a` but deny `_tidb_rowid` with 1143.
    for sql in [
        "SELECT a FROM test.t WHERE _tidb_rowid = 1",
        "SELECT a FROM test.t ORDER BY _tidb_rowid",
    ] {
        let err = plan_as(&tables, &handle, "u_cola", sql).unwrap_err();
        assert_eq!(err.code(), 1143, "u_cola on `{sql}`: {err}");
        let raw = err.error();
        let msg = String::from_utf8_lossy(&raw);
        assert!(
            msg.contains("for column '_tidb_rowid' in table 't'"),
            "u_cola on `{sql}`: {msg}"
        );
    }

    // Full-table (`u_tbl`), DB (`u_db_sel`), and global (`root`) SELECT users succeed.
    for user in ["u_tbl", "u_db_sel", "root"] {
        for sql in rowid_queries {
            plan_as(&tables, &handle, user, sql)
                .unwrap_or_else(|e| panic!("{user} should pass `{sql}`, got {e}"));
        }
    }
}
