//! Matrix tests for ruling R29 (specs/D04.1-privilege/spec.md §5.2 items 1–2 & 5, §6.1 D1):
//! unconditional single-point table-read `SELECT` (`column = "*"`, error `1142`) registration
//! at DataSource and PointGet boundaries, removal of Go's case-sensitive `collectPrivsForCount`
//! patch, and PointGet `IsTableDual` overflow WHERE-column preservation.

use std::sync::Arc;

use tidb_base::Error;
use tidb_base::gostd::time::utc;
use tidb_base::mysql::{NotNullFlag, PriKeyFlag, SQLMode, SelectPriv, TypeLong};
use tidb_model::{ColumnInfo, IndexColumn, IndexInfo, SequenceInfo, StatePublic, TableInfo};
use tidb_parser::ast::Stmt;
use tidb_parser::auth::UserIdentity;
use tidb_parser::model::CIStr;
use tidb_parser::types::FieldType;
use tidb_parser::{ParseOptions, parse};
use tidb_planner::PlanCtx;
use tidb_planner::core::optimizer::{check_privilege, visit_info4_priv_check};
use tidb_planner::core::plan_cache_utils::InfoSchemaLookup;
use tidb_planner::core::planbuilder::{VisitInfo, new_plan_builder};
use tidb_planner::core::point_get_plan::{get_visit_info, try_point_get_plan};
use tidb_planner::core::preprocess::{
    InfoSchemaHandle, PreprocessorReturn, preprocess, with_preprocessor_return,
};
use tidb_planner::core::resolve::new_node_w;
use tidb_privilege::{ColumnsPrivRecord, Handle, MySQLPrivilege, UserPrivileges, UserRecord};
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

fn mock_tables() -> Arc<MockIs> {
    // test.t(a INT, b INT)
    let t = Arc::new(TableInfo {
        id: 10,
        dbid: 1,
        name: CIStr::new("t"),
        columns: Some(vec![int_col(1, 0, "a", false), int_col(2, 1, "b", false)]),
        state: StatePublic,
        ..Default::default()
    });

    // test.t_pk(a INT PRIMARY KEY, b INT)
    let t_pk = Arc::new(TableInfo {
        id: 11,
        dbid: 1,
        name: CIStr::new("t_pk"),
        columns: Some(vec![int_col(1, 0, "a", true), int_col(2, 1, "b", false)]),
        pk_is_handle: true,
        state: StatePublic,
        ..Default::default()
    });

    // test.t_uk(id INT PRIMARY KEY, a INT, b INT, UNIQUE KEY uk_a(a))
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

    // test.seq (sequence)
    let seq = Arc::new(TableInfo {
        id: 13,
        dbid: 1,
        name: CIStr::new("seq"),
        sequence: Some(Box::new(SequenceInfo::default())),
        state: StatePublic,
        ..Default::default()
    });

    Arc::new(MockIs(vec![t, t_pk, t_uk, seq]))
}

fn build_visit_infos(
    tables: &Arc<MockIs>,
    sql: &str,
    user: Option<UserIdentity>,
    current_db: &str,
) -> Vec<VisitInfo> {
    let out = parse(sql, &ParseOptions::default()).unwrap_or_else(|e| panic!("parse {sql}: {e}"));
    let mut stmt = out.stmts.into_iter().next().unwrap();
    let mut vars = SessionVars::new(Box::new(Acc));
    vars.current_db = current_db.to_owned();
    vars.user = user;
    let is = InfoSchemaHandle(Arc::clone(tables) as Arc<dyn InfoSchemaLookup>);

    let resolve_ctx = {
        let mut node = new_node_w(&mut stmt);
        let (_, res) = preprocess(
            &mut vars,
            &mut node,
            vec![with_preprocessor_return(PreprocessorReturn {
                info_schema: Some(is.clone()),
                ..Default::default()
            })],
        );
        res.unwrap_or_else(|e| panic!("preprocess {sql}: {e}"));
        node.get_resolve_context()
    };

    let mut builder = new_plan_builder(&[]);
    builder.init(&mut vars, Some(is), None);
    builder.sql_text = sql;
    builder.resolve_ctx = Some(resolve_ctx);

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
    let mut ctx = PlanCtx::new(&mut vars, expr_ctx);
    ctx.has_dirty_content = Some(Arc::new(|_| false));
    builder
        .build(&mut ctx, &stmt)
        .unwrap_or_else(|e| panic!("build {sql}: {e}"));
    builder.get_visit_info().to_vec()
}

fn assert_first_is_table_select(
    vi: &[VisitInfo],
    db: &str,
    tbl: &str,
    user: &str,
    host: &str,
    sql: &str,
) {
    assert!(
        !vi.is_empty(),
        "expected table-read VisitInfo for `{sql}`, got empty"
    );
    let first = &vi[0];
    assert_eq!(first.privilege, SelectPriv.0, "{sql}");
    assert_eq!(first.db, db, "{sql}");
    assert_eq!(first.table, tbl, "{sql}");
    assert_eq!(first.column, "*", "{sql}");
    let err = first.err.as_ref().expect("table-read VisitInfo has error");
    assert_eq!(err.code(), 1142, "{sql}: {err}");
    let raw = err.error();
    let msg = String::from_utf8_lossy(&raw);
    assert!(
        msg.contains(&format!("'{user}'@'{host}'")) && msg.contains(&format!("for table '{tbl}'")),
        "{sql}: unexpected error message: {msg}"
    );
}

/// Pins ruling R29 across the spec §3.2 SQL matrix:
/// - Constant projections without column references (Go v8.5.8 produces empty `VisitInfo`)
/// - Uppercase / mixed-case `COUNT(*)` and non-`count` aggregates (Go v8.5.8 only matches lowercase `"count"`)
/// - Column references and wildcards (table-read `("*", 1142)` precedes column `1143` entries)
/// - Table aliases (real physical table name `t`, never alias `alias_t`)
/// - Sequences (table-read registered before `TableDual` conversion)
/// - Non-table queries (remain empty `VisitInfo`)
#[test]
fn r29_table_read_select_visit_info_matrix() {
    let tables = mock_tables();
    let u_none = Some(UserIdentity {
        username: "u_none".to_owned(),
        hostname: "127.0.0.1".to_owned(),
        ..Default::default()
    });

    // 1. Constant projections & predicates with zero column references (Go v8.5.8: vi == []).
    for sql in [
        "SELECT 1 FROM test.t",
        "SELECT 'x' FROM test.t LIMIT 1",
        "SELECT 1 FROM test.t WHERE 1=0",
        "SELECT 1 FROM test.t WHERE 1=1",
        "SELECT 1 FROM test.t AS alias_t",
        "SELECT 1 FROM (SELECT 1 FROM test.t) sub",
    ] {
        let vi = build_visit_infos(&tables, sql, u_none.clone(), "");
        assert_eq!(vi.len(), 1, "{sql}: expected single table-read VisitInfo");
        assert_first_is_table_select(&vi, "test", "t", "u_none", "127.0.0.1", sql);
    }

    // 2. Case-insensitive COUNT(*) and all other constant aggregates (Go v8.5.8: only lowercase `count` gets `*`).
    for sql in [
        "SELECT count(*) FROM test.t",
        "SELECT COUNT(*) FROM test.t",
        "SELECT Count(*) FROM test.t",
        "SELECT COUNT(1) FROM test.t",
        "SELECT count(1) FROM test.t",
        "SELECT sum(1) FROM test.t",
        "SELECT SUM(1) FROM test.t",
        "SELECT max(1) FROM test.t",
        "SELECT MAX(1) FROM test.t",
        "SELECT min(1) FROM test.t",
        "SELECT avg(1) FROM test.t",
        "SELECT group_concat('x') FROM test.t",
        "SELECT 1 FROM test.t HAVING COUNT(*) > 0",
        "SELECT COUNT(*), count(*), SUM(1) FROM test.t",
    ] {
        let vi = build_visit_infos(&tables, sql, u_none.clone(), "");
        // No duplicate "*" entries from removed `collect_privs_for_count`.
        let star_count = vi.iter().filter(|v| v.column == "*").count();
        assert_eq!(
            star_count, 1,
            "{sql}: expected exactly one '*' entry: {vi:?}"
        );
        assert_first_is_table_select(&vi, "test", "t", "u_none", "127.0.0.1", sql);
    }

    // 3. Column references & wildcards: table-read ("*", 1142) comes FIRST before column checks.
    {
        let vi = build_visit_infos(&tables, "SELECT a FROM test.t", u_none.clone(), "");
        assert_eq!(vi.len(), 2);
        assert_first_is_table_select(
            &vi,
            "test",
            "t",
            "u_none",
            "127.0.0.1",
            "SELECT a FROM test.t",
        );
        assert_eq!(vi[1].column, "a");
        assert_eq!(vi[1].err.as_ref().unwrap().code(), 1143);
    }
    {
        let vi = build_visit_infos(
            &tables,
            "SELECT 1 FROM test.t WHERE b = 1",
            u_none.clone(),
            "",
        );
        assert_eq!(vi.len(), 2);
        assert_first_is_table_select(&vi, "test", "t", "u_none", "127.0.0.1", "WHERE b = 1");
        assert_eq!(vi[1].column, "b");
        assert_eq!(vi[1].err.as_ref().unwrap().code(), 1143);
    }
    for sql in ["SELECT * FROM test.t", "TABLE test.t"] {
        let vi = build_visit_infos(&tables, sql, u_none.clone(), "");
        assert_eq!(vi.len(), 3, "{sql}");
        assert_first_is_table_select(&vi, "test", "t", "u_none", "127.0.0.1", sql);
        assert_eq!(vi[1].column, "a");
        assert_eq!(vi[1].err.as_ref().unwrap().code(), 1142);
        assert_eq!(vi[2].column, "b");
        assert_eq!(vi[2].err.as_ref().unwrap().code(), 1142);
    }

    // 4. Sequence converted to TableDual still registers table-read SelectPriv.
    {
        let vi = build_visit_infos(&tables, "SELECT 1 FROM test.seq", u_none.clone(), "");
        assert_eq!(vi.len(), 1);
        assert_first_is_table_select(
            &vi,
            "test",
            "seq",
            "u_none",
            "127.0.0.1",
            "SELECT 1 FROM test.seq",
        );
    }

    // 5. Non-table queries keep empty VisitInfo (no spurious privilege requirement).
    for sql in [
        "SELECT 1",
        "SELECT 1 FROM DUAL",
        "SELECT 1 FROM (SELECT 1 AS x) sub",
    ] {
        let vi = build_visit_infos(&tables, sql, u_none.clone(), "");
        assert!(vi.is_empty(), "{sql}: expected empty VisitInfo, got {vi:?}");
    }

    // 6. R29 GROUP BY column privileges & alias shadowing (owner decision 2026-11-28):
    //    Go v8.5.8 guarded `resolveGbyExprs` instead of `rewriteGbyExprs`, so GROUP BY column
    //    references (including alias-shadowing queries like `SELECT COUNT(*) AS b FROM t GROUP BY b`)
    //    skipped column-privilege checks. `tidb-rs` guards `rewrite_gby_exprs` on the new stack so
    //    a partial-column `SELECT(a)` user is rejected with 1143 whenever GROUP BY resolves to an
    //    ungranted table column `b`, while authorized columns and non-table aliases still pass.
    let mut mp = MySQLPrivilege::new();
    mp.add_user(UserRecord::new(
        "%",
        "u_cola",
        tidb_base::mysql::PrivilegeType(0),
    ));
    mp.add_column_priv(ColumnsPrivRecord::new(
        "u_cola", "%", "test", "t", "a", SelectPriv,
    ));
    let handle = Arc::new(Handle::new(mp));
    let pm = UserPrivileges::new("u_cola", "%", handle);
    let u_cola = Some(UserIdentity {
        username: "u_cola".to_owned(),
        hostname: "%".to_owned(),
        ..Default::default()
    });

    for denied_sql in [
        "SELECT COUNT(*) AS b FROM test.t GROUP BY b",
        "SELECT COUNT(*) FROM test.t GROUP BY b",
        "SELECT a + 1 AS b FROM test.t GROUP BY b",
    ] {
        let vi = build_visit_infos(&tables, denied_sql, u_cola.clone(), "test");
        assert!(
            vi.iter().any(|v| v.table == "t"
                && v.column == "b"
                && v.err.as_ref().is_some_and(|e| e.code() == 1143)),
            "{denied_sql}: expected column 'b' 1143 VisitInfo, got {vi:?}"
        );
        let filtered = visit_info4_priv_check(tables.as_ref(), None, &vi);
        let err = check_privilege(&[], &pm, &filtered)
            .expect_err(&format!("{denied_sql}: u_cola must be rejected with 1143"));
        assert_eq!(err.code(), 1143, "{denied_sql}: {err}");
        let raw = err.error();
        let msg = String::from_utf8_lossy(&raw);
        assert!(
            msg.contains("for column 'b' in table 't'"),
            "{denied_sql}: unexpected error message: {msg}"
        );
    }

    for allowed_sql in [
        "SELECT COUNT(*) FROM test.t GROUP BY a",
        "SELECT a AS c FROM test.t GROUP BY c",
        "SELECT 1 AS c FROM test.t GROUP BY c",
    ] {
        let vi = build_visit_infos(&tables, allowed_sql, u_cola.clone(), "test");
        let filtered = visit_info4_priv_check(tables.as_ref(), None, &vi);
        check_privilege(&[], &pm, &filtered)
            .unwrap_or_else(|e| panic!("{allowed_sql}: u_cola must be allowed, got {e}"));
    }
}

/// Pins R29 on the PointGet fast path (spec §2.2 item 9, §5.2 item 5):
/// - `get_visit_info` prepends table-read `("*", 1142)` before column entries.
/// - `try_point_get_plan` on secondary-unique-index `IsTableDual` overflow
///   (`WHERE a = 999999999999999999999 AND a = 1`) resolves lowercase `db_name` even when
///   unqualified and preserves `a` in `cols_in_where_clause` so `u_colb` cannot bypass `a`.
#[test]
fn r29_point_get_table_read_and_overflow_is_table_dual() {
    let tables = mock_tables();

    // Unqualified table name with secondary unique index + overflow inside LogicAnd.
    let sql = "SELECT b FROM t_uk WHERE a = 999999999999999999999 AND a = 1";
    let out = parse(sql, &ParseOptions::default()).unwrap();
    let mut stmt = out.stmts.into_iter().next().unwrap();
    let mut vars = SessionVars::new(Box::new(Acc));
    vars.current_db = "test".to_owned();
    vars.user = Some(UserIdentity {
        username: "u_colb".to_owned(),
        hostname: "%".to_owned(),
        ..Default::default()
    });
    let is = InfoSchemaHandle(Arc::clone(&tables) as Arc<dyn InfoSchemaLookup>);

    let resolve_ctx = {
        let mut node = new_node_w(&mut stmt);
        let (_, res) = preprocess(
            &mut vars,
            &mut node,
            vec![with_preprocessor_return(PreprocessorReturn {
                info_schema: Some(is),
                ..Default::default()
            })],
        );
        res.unwrap();
        node.get_resolve_context()
    };

    let ev = es::new_eval_context(vec![
        es::with_type_flags(tidb_types::context::DefaultStmtFlags),
        es::with_sql_mode(SQLMode(0)),
        es::with_location(Some(utc())),
        es::with_current_db("test".to_owned()),
    ]);
    let expr_ctx = es::new_expr_context(vec![
        es::with_eval_ctx(ev),
        es::with_charset("utf8mb4".to_owned(), "utf8mb4_bin".to_owned()),
        es::with_default_collation_for_utf8mb4("utf8mb4_bin".to_owned()),
    ]);
    let mut ctx = PlanCtx::new(&mut vars, expr_ctx);
    let Stmt::Select(sel) = &stmt else {
        panic!("expected SELECT");
    };
    let fp = try_point_get_plan(&mut ctx, sel, &resolve_ctx, false)
        .unwrap()
        .expect("secondary-unique-index overflow builds IsTableDual PointGetPlan");
    assert!(fp.is_table_dual);
    // Spec §5.2 item 5: resolved db_name is "test", NOT empty tbl_name.schema.o.
    assert_eq!(fp.db_name, "test");
    // Spec §5.2 item 5: WHERE column `a` is preserved despite LogicAnd overflow.
    assert_eq!(fp.cols_in_where_clause, vec!["a".to_owned()]);

    let vi = get_visit_info(
        "u_colb",
        "%",
        &fp.db_name,
        &fp.tbl_info.name.l,
        &fp.output_names,
        &fp.unfold_from_wild_card,
        &fp.cols_in_where_clause,
    )
    .expect("visit infos");
    assert_eq!(vi.len(), 3);
    assert_first_is_table_select(&vi, "test", "t_uk", "u_colb", "%", sql);
    assert_eq!(vi[1].column, "b");
    assert_eq!(vi[1].err.as_ref().unwrap().code(), 1143);
    assert_eq!(vi[2].column, "a");
    assert_eq!(vi[2].err.as_ref().unwrap().code(), 1143);
}
