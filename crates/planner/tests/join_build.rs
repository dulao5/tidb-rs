//! Plan-level tests of the join builder (block D09.3 part A, `specs/D09.3-join-build/spec.md`
//! §6.3): the logical plan shape, the schema, the NOT NULL flags and the full schema of each
//! join type; ON / USING / NATURAL; the error order and the restore of `checkColPriv`; the
//! boundary stubs of `LogicalJoin`; and the privilege rulings R29 / R32 on joins.

use std::sync::Arc;

use tidb_base::Error;
use tidb_base::blocked::blocked_by;
use tidb_base::errors::RedactLogDisable;
use tidb_base::gostd::time::utc;
use tidb_base::mysql::{
    NotNullFlag, PrivilegeType, SQLMode, SelectPriv, TypeLong, has_not_null_flag,
};
use tidb_kv::checker::RequestTypeSupportedChecker;
use tidb_kv::kv::{Client, ClientSendOption, Request, Response};
use tidb_model::{ColumnInfo, StatePublic, TableInfo};
use tidb_parser::auth::UserIdentity;
use tidb_parser::model::CIStr;
use tidb_parser::types::FieldType;
use tidb_parser::{ParseOptions, parse};
use tidb_planner::core::expression_rewriter::CheckSelectColumnPrivOption;
use tidb_planner::core::plan_cache_utils::{InfoSchemaLookup, table_is_view};
use tidb_planner::core::planbuilder::{PlanBuilder, VisitInfo, new_plan_builder};
use tidb_planner::core::preprocess::{
    InfoSchemaHandle, PreprocessorReturn, preprocess, with_preprocessor_return,
    with_privilege_manager,
};
use tidb_planner::core::resolve::new_node_w;
use tidb_planner::logicalop::{JoinType, LogicalJoin, LogicalSelection};
use tidb_planner::optimize::optimize_ast_node;
use tidb_planner::planctx::SessionPlanCtx;
use tidb_planner::property::PhysicalProperty;
use tidb_planner::util::optimizetrace::default_logical_optimize_option;
use tidb_planner::{LogicalKind, PlanCtx, PlanId, with_detached};
use tidb_privilege::{
    ColumnsPrivRecord, Handle, MySQLPrivilege, PrivilegeManagerRef, TablesPrivRecord,
    UserPrivileges, UserRecord,
};
use tidb_sessionctx::exprstatic as es;
use tidb_sessionctx::variable::session::{GlobalVarAccessor, SessionVars};
use tidb_types::field_name::NameSlice;

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

fn int_col(id: i64, offset: isize, name: &str, not_null: bool) -> Arc<ColumnInfo> {
    let mut ft = FieldType::new(TypeLong);
    ft.set_flen(11);
    if not_null {
        ft.set_flag(NotNullFlag);
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

fn table(id: i64, name: &str, cols: Vec<Arc<ColumnInfo>>) -> Arc<TableInfo> {
    Arc::new(TableInfo {
        id,
        dbid: 1,
        name: CIStr::new(name),
        columns: Some(cols),
        state: StatePublic,
        ..Default::default()
    })
}

/// test.t1(a INT NOT NULL, b INT), test.t2(a INT NOT NULL, b INT NOT NULL),
/// test.t3(a INT, c INT); none has a primary key, so each has `_tidb_rowid`. test.v1(a, b)
/// is a view (only looked up by name: views are not built yet).
fn tables() -> Arc<MockIs> {
    Arc::new(MockIs(vec![
        table(
            10,
            "t1",
            vec![int_col(1, 0, "a", true), int_col(2, 1, "b", false)],
        ),
        table(
            11,
            "t2",
            vec![int_col(1, 0, "a", true), int_col(2, 1, "b", true)],
        ),
        table(
            12,
            "t3",
            vec![int_col(1, 0, "a", false), int_col(2, 1, "c", false)],
        ),
        Arc::new(TableInfo {
            view: Some(Box::default()),
            ..(*table(
                13,
                "v1",
                vec![int_col(1, 0, "a", false), int_col(2, 1, "b", false)],
            ))
            .clone()
        }),
    ]))
}

fn new_vars(user: Option<UserIdentity>) -> SessionVars {
    let mut vars = SessionVars::new(Box::new(Acc));
    vars.current_db = "test".to_owned();
    vars.user = user;
    vars
}

fn new_expr_ctx() -> Arc<tidb_sessionctx::exprstatic::ExprContext> {
    let ev = es::new_eval_context(vec![
        es::with_type_flags(tidb_types::context::DefaultStmtFlags),
        es::with_sql_mode(SQLMode(0)),
        es::with_location(Some(utc())),
        es::with_current_db("test".to_owned()),
    ]);
    es::new_expr_context(vec![
        es::with_eval_ctx(ev),
        es::with_charset("utf8mb4".to_owned(), "utf8mb4_bin".to_owned()),
        es::with_default_collation_for_utf8mb4("utf8mb4_bin".to_owned()),
    ])
}

/// Builds the logical plan of `sql` (as user `u` with no privileges, so that the visit
/// information carries the errors) and hands the builder, the plan context and the result
/// to `f`.
fn with_plan<R>(
    sql: &str,
    f: impl FnOnce(&PlanBuilder<'_>, &mut PlanCtx<'_>, Result<PlanId, Error>) -> R,
) -> R {
    let tables = tables();
    let out = parse(sql, &ParseOptions::default()).unwrap_or_else(|e| panic!("parse {sql}: {e}"));
    let mut stmt = out.stmts.into_iter().next().unwrap();
    let mut vars = new_vars(Some(UserIdentity {
        username: "u".to_owned(),
        hostname: "%".to_owned(),
        ..Default::default()
    }));
    let is = InfoSchemaHandle(Arc::clone(&tables) as Arc<dyn InfoSchemaLookup>);
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
    let mut ctx = PlanCtx::new(&mut vars, new_expr_ctx());
    ctx.has_dirty_content = Some(Arc::new(|_| false));
    let res = builder.build(&mut ctx, &stmt);
    f(&builder, &mut ctx, res)
}

fn build_ok<R>(sql: &str, f: impl FnOnce(&mut PlanCtx<'_>, PlanId) -> R) -> R {
    with_plan(sql, |_, ctx, res| {
        let root = res.unwrap_or_else(|e| panic!("build {sql}: {e}"));
        f(ctx, root)
    })
}

fn build_err(sql: &str) -> (Error, CheckSelectColumnPrivOption) {
    with_plan(sql, |b, _, res| {
        let err = res.expect_err(&format!("`{sql}` must fail"));
        (err, b.check_col_priv)
    })
}

fn kinds(ctx: &PlanCtx<'_>, root: PlanId) -> Vec<LogicalKind> {
    let mut out = Vec::new();
    let mut p = root;
    loop {
        let node = ctx.arena.get(p);
        out.push(node.kind());
        match node.children() {
            [c] => p = *c,
            _ => return out,
        }
    }
}

/// The first join below `root`, walking the first child.
fn find_join(ctx: &PlanCtx<'_>, root: PlanId) -> PlanId {
    let mut p = root;
    loop {
        if ctx.arena.get(p).kind() == LogicalKind::Join {
            return p;
        }
        p = ctx.arena.get(p).children()[0];
    }
}

fn join_of<'a>(ctx: &'a PlanCtx<'_>, id: PlanId) -> &'a LogicalJoin {
    ctx.arena.downcast_ref::<LogicalJoin>(id).unwrap()
}

fn names(names: &NameSlice) -> Vec<String> {
    names
        .iter()
        .map(|n| format!("{}.{}", n.tbl_name.l, n.col_name.l))
        .collect()
}

fn redundant(names: &NameSlice) -> Vec<bool> {
    names.iter().map(|n| n.redundant).collect()
}

fn not_null(schema: &tidb_planner::Schema) -> Vec<bool> {
    schema
        .columns
        .iter()
        .map(|c| has_not_null_flag(c.ret_type.get_flag()))
        .collect()
}

fn exprs(ctx: &PlanCtx<'_>, es: &[tidb_expression::ExprRef]) -> Vec<String> {
    let ectx = ctx.eval_ctx();
    es.iter()
        .map(|e| String::from_utf8_lossy(&e.string_with_ctx(&*ectx, RedactLogDisable)).into_owned())
        .collect()
}

fn msg(err: &Error) -> String {
    String::from_utf8_lossy(&err.error()).into_owned()
}

/// An inner join with ON keeps the ON conditions in a selection over the join (Go
/// `buildJoin:929-935`); the join keeps both schemas and their NOT NULL flags.
#[test]
fn inner_join_on_is_a_selection_over_the_join() {
    build_ok("SELECT * FROM t1 JOIN t2 ON t1.a = t2.a", |ctx, root| {
        assert_eq!(
            kinds(ctx, root),
            vec![
                LogicalKind::Projection,
                LogicalKind::Selection,
                LogicalKind::Join
            ]
        );
        let sel_id = ctx.arena.get(root).children()[0];
        let sel = ctx.arena.downcast_ref::<LogicalSelection>(sel_id).unwrap();
        assert_eq!(
            exprs(ctx, &sel.conditions),
            vec!["eq(test.t1.a, test.t2.a)"]
        );
        let j = join_of(ctx, find_join(ctx, root));
        assert_eq!(j.join_type, JoinType::InnerJoin);
        assert!(!j.cartesian_join && !j.straight_join);
        assert!(j.equal_conditions.is_empty() && j.other_conditions.is_empty());
        let schema = ctx.arena.get(find_join(ctx, root)).schema(&ctx.arena);
        // The hidden `_tidb_rowid` of each table is in the schema (NOT NULL).
        assert_eq!(not_null(schema), vec![true, false, true, true, true, true]);
        assert_eq!(
            names(&j.full_names),
            vec![
                "t1.a",
                "t1.b",
                "t1._tidb_rowid",
                "t2.a",
                "t2.b",
                "t2._tidb_rowid"
            ]
        );
        assert_eq!(redundant(&j.full_names), vec![false; 6]);
    });
}

/// Joins without conditions are cartesian inner joins, whatever their spelling.
#[test]
fn join_without_condition_is_cartesian() {
    for sql in [
        "SELECT * FROM t1, t2",
        "SELECT * FROM t1 JOIN t2",
        "SELECT * FROM t1 CROSS JOIN t2",
        "SELECT * FROM t1 INNER JOIN t2",
    ] {
        build_ok(sql, |ctx, root| {
            assert_eq!(
                kinds(ctx, root),
                vec![LogicalKind::Projection, LogicalKind::Join],
                "{sql}"
            );
            let j = join_of(ctx, find_join(ctx, root));
            assert_eq!(j.join_type, JoinType::InnerJoin, "{sql}");
            assert!(j.cartesian_join, "{sql}");
        });
    }
    build_ok("SELECT STRAIGHT_JOIN * FROM t1, t2", |ctx, root| {
        assert!(join_of(ctx, find_join(ctx, root)).straight_join);
    });
}

/// LEFT JOIN: the ON conditions are attached to the join, the inner (right) side loses NOT
/// NULL in the schema and in the full schema.
#[test]
fn left_join_clears_not_null_of_the_inner_side() {
    build_ok(
        "SELECT * FROM t1 LEFT JOIN t2 ON t1.a = t2.a AND t2.b > 1 AND t1.b < 2",
        |ctx, root| {
            assert_eq!(
                kinds(ctx, root),
                vec![LogicalKind::Projection, LogicalKind::Join]
            );
            let id = find_join(ctx, root);
            let j = join_of(ctx, id);
            assert_eq!(j.join_type, JoinType::LeftOuterJoin);
            assert!(!j.cartesian_join);
            assert_eq!(
                exprs(ctx, &j.equal_conditions),
                vec!["eq(test.t1.a, test.t2.a)"]
            );
            assert_eq!(exprs(ctx, &j.right_conditions), vec!["gt(test.t2.b, 1)"]);
            assert_eq!(exprs(ctx, &j.left_conditions), vec!["lt(test.t1.b, 2)"]);
            let schema = ctx.arena.get(id).schema(&ctx.arena);
            assert_eq!(
                not_null(schema),
                vec![true, false, true, false, false, false]
            );
            assert_eq!(
                not_null(j.full_schema.as_ref().unwrap()),
                vec![true, false, true, false, false, false]
            );
        },
    );
}

/// RIGHT JOIN: the inner (left) side loses NOT NULL; the full schema is ordered outer first.
#[test]
fn right_join_clears_not_null_of_the_inner_side() {
    build_ok(
        "SELECT * FROM t1 RIGHT JOIN t2 ON t1.a = t2.a",
        |ctx, root| {
            let id = find_join(ctx, root);
            let j = join_of(ctx, id);
            assert_eq!(j.join_type, JoinType::RightOuterJoin);
            let schema = ctx.arena.get(id).schema(&ctx.arena);
            assert_eq!(
                not_null(schema),
                vec![false, false, false, true, true, true]
            );
            assert_eq!(
                names(&j.full_names),
                vec![
                    "t2.a",
                    "t2.b",
                    "t2._tidb_rowid",
                    "t1.a",
                    "t1.b",
                    "t1._tidb_rowid"
                ]
            );
            assert_eq!(
                not_null(j.full_schema.as_ref().unwrap()),
                vec![true, true, true, false, false, false]
            );
        },
    );
}

/// USING: the common column comes first, the right one is redundant in the full names, and
/// the EQ condition is an "other" condition (Go `coalesceCommonColumns:1320-1340`).
#[test]
fn using_coalesces_the_common_columns() {
    build_ok("SELECT * FROM t1 JOIN t2 USING (b)", |ctx, root| {
        let id = find_join(ctx, root);
        let j = join_of(ctx, id);
        assert_eq!(
            names(ctx.arena.get(id).output_names(&ctx.arena)),
            vec!["t1.b", "t1.a", "t1._tidb_rowid", "t2.a", "t2._tidb_rowid"]
        );
        assert_eq!(
            exprs(ctx, &j.other_conditions),
            vec!["eq(test.t1.b, test.t2.b)"]
        );
        assert_eq!(
            names(&j.full_names),
            vec![
                "t1.a",
                "t1.b",
                "t1._tidb_rowid",
                "t2.a",
                "t2.b",
                "t2._tidb_rowid"
            ]
        );
        assert_eq!(
            redundant(&j.full_names),
            vec![false, false, false, false, true, false]
        );
        assert!(!j.cartesian_join);
        // The projection outputs the coalesced schema.
        assert_eq!(
            names(ctx.arena.get(root).output_names(&ctx.arena)),
            vec!["t1.b", "t1.a", "t2.a"]
        );
    });
}

/// RIGHT JOIN ... USING: the right table's columns come first; the EQ condition's left
/// argument is still the original left table's column, and the redundant mark is on it
/// (spec §4.2 item 7; Go `:1323, 1331`).
#[test]
fn coalesce_common_columns_right_join_keeps_left_as_lc() {
    build_ok("SELECT * FROM t1 RIGHT JOIN t2 USING (a)", |ctx, root| {
        let id = find_join(ctx, root);
        let j = join_of(ctx, id);
        assert_eq!(j.join_type, JoinType::RightOuterJoin);
        assert_eq!(
            names(ctx.arena.get(id).output_names(&ctx.arena)),
            vec!["t2.a", "t2.b", "t2._tidb_rowid", "t1.b", "t1._tidb_rowid"]
        );
        let schema = ctx.arena.get(id).schema(&ctx.arena);
        assert_eq!(not_null(schema), vec![true, true, true, false, false]);
        assert_eq!(
            exprs(ctx, &j.other_conditions),
            vec!["eq(test.t1.a, test.t2.a)"]
        );
        assert_eq!(
            names(&j.full_names),
            vec![
                "t2.a",
                "t2.b",
                "t2._tidb_rowid",
                "t1.a",
                "t1.b",
                "t1._tidb_rowid"
            ]
        );
        assert_eq!(
            redundant(&j.full_names),
            vec![false, false, false, true, false, false]
        );
    });
}

/// NATURAL JOIN coalesces every common visible column, never `_tidb_rowid`.
#[test]
fn natural_join_coalesces_all_common_columns() {
    build_ok("SELECT * FROM t1 NATURAL JOIN t2", |ctx, root| {
        let id = find_join(ctx, root);
        let j = join_of(ctx, id);
        assert_eq!(
            names(ctx.arena.get(id).output_names(&ctx.arena)),
            vec!["t1.a", "t1.b", "t1._tidb_rowid", "t2._tidb_rowid"]
        );
        assert_eq!(
            exprs(ctx, &j.other_conditions),
            vec!["eq(test.t1.a, test.t2.a)", "eq(test.t1.b, test.t2.b)"]
        );
        assert_eq!(
            redundant(&j.full_names),
            vec![false, false, false, true, true, false]
        );
    });
    build_ok("SELECT * FROM t1 NATURAL LEFT JOIN t3", |ctx, root| {
        let id = find_join(ctx, root);
        let j = join_of(ctx, id);
        assert_eq!(j.join_type, JoinType::LeftOuterJoin);
        assert_eq!(
            names(ctx.arena.get(id).output_names(&ctx.arena)),
            vec!["t1.a", "t1.b", "t1._tidb_rowid", "t3.c", "t3._tidb_rowid"]
        );
        assert_eq!(
            exprs(ctx, &j.other_conditions),
            vec!["eq(test.t1.a, test.t3.a)"]
        );
    });
}

/// The redundant column of a USING join can be selected, also through a selection, and a
/// qualified wildcard expands from the full schema.
#[test]
fn redundant_columns_resolve_through_the_full_schema() {
    build_ok(
        "SELECT t2.a FROM t1 JOIN t2 USING (a) WHERE t2.a > 0",
        |ctx, root| {
            assert_eq!(
                names(ctx.arena.get(root).output_names(&ctx.arena)),
                vec!["t2.a"]
            );
        },
    );
    build_ok(
        "SELECT t1.*, t2.* FROM t1 JOIN t2 USING (a)",
        |ctx, root| {
            assert_eq!(
                names(ctx.arena.get(root).output_names(&ctx.arena)),
                vec!["t1.a", "t1.b", "t2.a", "t2.b"]
            );
        },
    );
    build_ok(
        "SELECT t2.a FROM t1 JOIN t2 USING (a) ORDER BY t2.a",
        |ctx, root| {
            assert_eq!(
                names(ctx.arena.get(root).output_names(&ctx.arena)),
                vec!["t2.a"]
            );
        },
    );
    build_ok(
        "SELECT t2.a, COUNT(*) FROM t1 JOIN t2 USING (a) GROUP BY t2.a",
        |ctx, root| {
            assert_eq!(
                names(ctx.arena.get(root).output_names(&ctx.arena))[0],
                "t2.a"
            );
        },
    );
}

/// An unknown USING column (`_tidb_rowid` included, which never coalesces) is
/// ErrUnknownColumn, and `checkColPriv` is restored by the drop guard (Go's `defer`).
#[test]
fn check_col_priv_restored_after_using_error() {
    for (sql, col) in [
        ("SELECT * FROM t1 JOIN t2 USING (c)", "c"),
        (
            "SELECT * FROM t1 JOIN t2 USING (_tidb_rowid)",
            "_tidb_rowid",
        ),
        ("SELECT * FROM t1 JOIN t3 USING (a, b)", "b"),
    ] {
        let (err, priv_opt) = build_err(sql);
        assert_eq!(err.code(), 1054, "{sql}: {err}");
        assert_eq!(
            msg(&err),
            format!("[planner:1054]Unknown column '{col}' in 'from clause'"),
            "{sql}"
        );
        assert_eq!(
            priv_opt,
            CheckSelectColumnPrivOption::NoCheckOption,
            "{sql}"
        );
    }
}

/// An error in the ON condition is returned after `checkColPriv` is restored.
#[test]
fn check_col_priv_restored_after_on_error() {
    let (err, priv_opt) = build_err("SELECT * FROM t1 LEFT JOIN t2 ON t1.zz = t2.a");
    assert_eq!(err.code(), 1054, "{err}");
    assert_eq!(
        msg(&err),
        "[planner:1054]Unknown column 't1.zz' in 'on clause'"
    );
    assert_eq!(priv_opt, CheckSelectColumnPrivOption::NoCheckOption);
}

/// Same-named columns on one side: ErrAmbiguous for USING and NATURAL.
#[test]
fn ambiguous_common_columns() {
    for sql in [
        "SELECT * FROM (t1 JOIN t2) JOIN t3 USING (a)",
        "SELECT * FROM t3 JOIN (t1 JOIN t2) USING (a)",
        "SELECT * FROM (t1 JOIN t2) NATURAL JOIN t3",
        "SELECT * FROM t3 NATURAL JOIN (t1 JOIN t2)",
    ] {
        let (err, _) = build_err(sql);
        assert_eq!(err.code(), 1052, "{sql}: {err}");
        assert_eq!(
            msg(&err),
            "[planner:1052]Column 'a' in from clause is ambiguous",
            "{sql}"
        );
    }
}

/// `FULL OUTER JOIN` (which v8.5.8's grammar rejects) stops at the parser's boundary stub
/// instead of being built as an inner join.
#[test]
fn full_join_is_blocked_not_inner() {
    let (err, _) = build_err("SELECT * FROM t1 FULL OUTER JOIN t2 ON t1.a = t2.a");
    assert_eq!(blocked_by(&err), Some("D02.2"), "{err}");
}

/// Every interface method Go's `LogicalJoin` overrides is explicit: the ones of later
/// blocks return their block's `blocked` error, never the `BaseLogicalPlan` default.
#[test]
fn join_trait_methods_are_explicit() {
    build_ok(
        "SELECT * FROM t1 LEFT JOIN t2 ON t1.a = t2.a",
        |ctx, root| {
            let id = find_join(ctx, root);
            let opt = default_logical_optimize_option();
            let block =
                |r: Result<(), Error>| r.err().and_then(|e| blocked_by(&e).map(str::to_owned));
            let ppd = with_detached(ctx, id, |n, ctx| {
                n.predicate_push_down(ctx, id, Vec::new(), &opt).map(|_| ())
            });
            assert_eq!(block(ppd).as_deref(), Some("D09.8"));
            let prune = with_detached(ctx, id, |n, ctx| {
                n.prune_columns(ctx, id, Vec::new(), &opt).map(|_| ())
            });
            assert_eq!(block(prune).as_deref(), Some("D09.7"));
            let topn = with_detached(ctx, id, |n, ctx| {
                n.push_down_top_n(ctx, id, None, &opt).map(|_| ())
            });
            assert_eq!(block(topn).as_deref(), Some("D09.11"));
            let cp = with_detached(ctx, id, |n, ctx| {
                n.constant_propagation(ctx, None, 0, &opt).map(|_| ())
            });
            assert_eq!(block(cp).as_deref(), Some("D09.8"));
            let o2i = with_detached(ctx, id, |n, ctx| {
                n.convert_outer_to_inner_join(ctx, id, Vec::new())
                    .map(|_| ())
            });
            assert_eq!(block(o2i).as_deref(), Some("D09.8"));
            let exhaust = with_detached(ctx, id, |n, ctx| {
                n.exhaust_physical_plans(ctx, id, &PhysicalProperty::default())
                    .map(|_| ())
            });
            assert_eq!(block(exhaust).as_deref(), Some("D10.3"));
            let stats = with_detached(ctx, id, |n, ctx| {
                n.derive_stats(ctx, id, &[], &[]).map(|_| ())
            });
            assert_eq!(block(stats).as_deref(), Some("D10.6"));
            let explain = ctx.arena.get(id).explain_info(ctx).map(|_| ());
            assert_eq!(block(explain).as_deref(), Some("D10.9"));
            let fd = join_of(ctx, id).extract_fd();
            assert_eq!(block(fd).as_deref(), Some("D09.14"));
            // Ported with the join: no correlated columns in a plain join.
            assert!(ctx.arena.get(id).extract_correlated_cols().is_empty());
        },
    );
}

/// R29: a join registers the table-read `("*", 1142)` once per base table, from each data
/// source, and nothing for the join itself.
#[test]
fn r29_one_table_read_per_base_table() {
    for sql in [
        "SELECT 1 FROM t1 JOIN t2 ON t1.a = t2.a",
        "SELECT 1 FROM t1 LEFT JOIN t2 USING (a)",
        "SELECT 1 FROM t1 NATURAL JOIN t2",
        "SELECT 1 FROM t1, t2",
    ] {
        let vi: Vec<VisitInfo> = with_plan(sql, |b, _, res| {
            res.unwrap_or_else(|e| panic!("build {sql}: {e}"));
            b.get_visit_info().to_vec()
        });
        let stars: Vec<&str> = vi
            .iter()
            .filter(|v| v.column == "*")
            .map(|v| v.table.as_str())
            .collect();
        assert_eq!(stars, vec!["t1", "t2"], "{sql}: {vi:?}");
        for v in vi.iter().filter(|v| v.column == "*") {
            assert_eq!(v.privilege, SelectPriv.0, "{sql}");
            assert_eq!(v.err.as_ref().unwrap().code(), 1142, "{sql}");
        }
    }
}

/// USING / NATURAL register the column privilege of both sides of each common column
/// (Go `coalesceCommonColumns:1281-1297`), as column errors (1143).
#[test]
fn using_registers_both_sides_of_common_columns() {
    let vi: Vec<VisitInfo> = with_plan("SELECT 1 FROM t1 JOIN t2 USING (b)", |b, _, res| {
        res.unwrap();
        b.get_visit_info().to_vec()
    });
    let cols: Vec<(String, String, i32)> = vi
        .iter()
        .filter(|v| v.column != "*")
        .map(|v| {
            (
                v.table.clone(),
                v.column.clone(),
                v.err.as_ref().map_or(0, |e| e.code() as i32),
            )
        })
        .collect();
    assert_eq!(
        cols,
        vec![
            ("t1".to_owned(), "b".to_owned(), 1143),
            ("t2".to_owned(), "b".to_owned(), 1143)
        ]
    );
}

fn priv_handle() -> Arc<Handle> {
    let mut mp = MySQLPrivilege::new();
    mp.add_user(UserRecord::new("%", "root", SelectPriv));
    mp.add_user(UserRecord::new("%", "u_cola", PrivilegeType(0)));
    for t in ["t1", "t2"] {
        mp.add_column_priv(ColumnsPrivRecord::new(
            "u_cola", "%", "test", t, "a", SelectPriv,
        ));
    }
    mp.add_user(UserRecord::new("%", "u_t1", PrivilegeType(0)));
    mp.add_table_priv(TablesPrivRecord::new("u_t1", "%", "test", "t1", SelectPriv));
    // SELECT(a) on t1 and t2 and the whole of t3.
    mp.add_user(UserRecord::new("%", "u_cola_t3", PrivilegeType(0)));
    for t in ["t1", "t2"] {
        mp.add_column_priv(ColumnsPrivRecord::new(
            "u_cola_t3",
            "%",
            "test",
            t,
            "a",
            SelectPriv,
        ));
    }
    mp.add_table_priv(TablesPrivRecord::new(
        "u_cola_t3",
        "%",
        "test",
        "t3",
        SelectPriv,
    ));
    // SELECT(a) on t1, t2 and t3.
    mp.add_user(UserRecord::new("%", "u_a3", PrivilegeType(0)));
    for t in ["t1", "t2", "t3"] {
        mp.add_column_priv(ColumnsPrivRecord::new(
            "u_a3", "%", "test", t, "a", SelectPriv,
        ));
    }
    Arc::new(Handle::new(mp))
}

/// Plans `sql` through the optimizer entry as `user`: the privilege check runs after the
/// build, before the logical optimization that a join stops in (a later block's stub).
fn plan_as(user: &str, sql: &str) -> Result<(), Error> {
    let tables = tables();
    let handle = priv_handle();
    let out = parse(sql, &ParseOptions::default()).unwrap_or_else(|e| panic!("parse {sql}: {e}"));
    let mut stmt = out.stmts.into_iter().next().unwrap();
    let mut vars = new_vars(Some(UserIdentity {
        username: user.to_owned(),
        hostname: "%".to_owned(),
        ..Default::default()
    }));
    let pm: PrivilegeManagerRef = Arc::new(UserPrivileges::new(user, "%", Arc::clone(&handle)));
    let is_dyn: Arc<dyn InfoSchemaLookup> = Arc::clone(&tables) as Arc<dyn InfoSchemaLookup>;
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
    let parts = SessionPlanCtx {
        expr_ctx: new_expr_ctx(),
        client: Some(Arc::new(MockClient)),
        has_dirty_content: Some(Arc::new(|_| false)),
        advise_txn_warmup: None,
        domain_info_schema: Some(Arc::clone(&is_dyn)),
        instance_plan_cache: None,
        privilege_manager: Some(pm),
    };
    optimize_ast_node(&parts, &mut vars, &mut node_w, &*is_dyn).map(|_| ())
}

/// The privilege errors of joins come from the plan build, before the join stops at a later
/// block's stub: 1143 for a USING column on either side (both are registered), 1143 for
/// `_tidb_rowid` of a joined table (R32), 1142 for a table without any privilege.
#[test]
fn join_privilege_errors_before_the_boundary() {
    // SELECT(a) on t1 and t2: the USING column b is denied (t1's side first).
    let err = plan_as("u_cola", "SELECT t1.a FROM t1 JOIN t2 USING (b)").unwrap_err();
    assert_eq!(err.code(), 1143, "{err}");
    assert!(
        msg(&err).contains("for column 'b' in table 't1'"),
        "{}",
        msg(&err)
    );
    // R32: _tidb_rowid of a joined table needs the whole table's SELECT.
    let err = plan_as(
        "u_cola",
        "SELECT t1._tidb_rowid FROM t1 JOIN t2 ON t1.a = t2.a",
    )
    .unwrap_err();
    assert_eq!(err.code(), 1143, "{err}");
    assert!(
        msg(&err).contains("for column '_tidb_rowid' in table 't1'"),
        "{}",
        msg(&err)
    );
    // u_t1 has no privilege on t2: 1142 for t2.
    let err = plan_as("u_t1", "SELECT 1 FROM t1 JOIN t2 ON t1.a = t2.a").unwrap_err();
    assert_eq!(err.code(), 1142, "{err}");
    assert!(msg(&err).contains("for table 't2'"), "{}", msg(&err));
    // With the privileges, the join passes the check and stops at a later block's boundary
    // stub (1235), so that the router falls back.
    for (user, sql) in [
        ("u_cola", "SELECT t1.a FROM t1 JOIN t2 USING (a)"),
        ("root", "SELECT * FROM t1 LEFT JOIN t2 ON t1.a = t2.a"),
        (
            "root",
            "SELECT t1._tidb_rowid FROM t1 JOIN t2 ON t1.a = t2.a",
        ),
    ] {
        let err = plan_as(user, sql).unwrap_err();
        assert!(blocked_by(&err).is_some(), "{user} `{sql}`: {err}");
    }
}

/// R30 holds for the whole statement: a table placed after a joined subtree answers 1142
/// before any column of the subtree answers 1143, whether the table exists (`t3`) or not
/// (`nosuch`, ruling R31 in preprocessing), so the answer tells nothing about the table's
/// existence. The left subtree registers its USING / ON column entries before the right data
/// source registers its table-read entry, so slice order alone would answer 1143.
#[test]
fn r30_table_denied_before_column_denied_across_joins() {
    let cases = [
        "SELECT 1 FROM t1 JOIN t2 USING (b) JOIN {t}",
        "SELECT 1 FROM t1 JOIN t2 ON t1.b = t2.b JOIN {t} ON {t}.a = t1.a",
        "SELECT 1 FROM t1 NATURAL JOIN t2 JOIN {t}",
    ];
    // SELECT(a) on t1 and t2, nothing on t3 or nosuch.
    for case in cases {
        for t in ["t3", "nosuch"] {
            let sql = case.replace("{t}", t);
            let err = plan_as("u_cola", &sql).unwrap_err();
            assert_eq!(err.code(), 1142, "`{sql}`: {err}");
            assert!(
                msg(&err).contains(&format!("for table '{t}'")),
                "`{sql}`: {}",
                msg(&err)
            );
        }
    }
    // With the read access to t3, the column error of the subtree stands.
    for case in cases {
        let sql = case.replace("{t}", "t3");
        let err = plan_as("u_cola_t3", &sql).unwrap_err();
        assert_eq!(err.code(), 1143, "`{sql}`: {err}");
        assert!(
            msg(&err).contains("for column 'b' in table 't1'"),
            "`{sql}`: {}",
            msg(&err)
        );
    }
}

/// R32: `_tidb_rowid` registers the full-table entry of the table it is qualified with, on
/// either side of a join, and of no other table.
#[test]
fn r32_rowid_entry_names_the_referenced_side() {
    for (sql, tbl) in [
        ("SELECT t1._tidb_rowid FROM t1 JOIN t2 ON t1.a = t2.a", "t1"),
        ("SELECT t2._tidb_rowid FROM t1 JOIN t2 ON t1.a = t2.a", "t2"),
        (
            "SELECT t3._tidb_rowid FROM t1 JOIN t2 ON t1.a = t2.a JOIN t3 ON t3.a = t1.a",
            "t3",
        ),
    ] {
        let vi: Vec<VisitInfo> = with_plan(sql, |b, _, res| {
            res.unwrap_or_else(|e| panic!("build {sql}: {e}"));
            b.get_visit_info().to_vec()
        });
        let rowid: Vec<&str> = vi
            .iter()
            .filter(|v| v.column.is_empty())
            .map(|v| v.table.as_str())
            .collect();
        assert_eq!(rowid, vec![tbl], "{sql}: {vi:?}");
        // SELECT(a) on every table: 1143 for `_tidb_rowid` of the referenced table.
        let err = plan_as("u_a3", sql).unwrap_err();
        assert_eq!(err.code(), 1143, "`{sql}`: {err}");
        assert!(
            msg(&err).contains(&format!("for column '_tidb_rowid' in table '{tbl}'")),
            "`{sql}`: {}",
            msg(&err)
        );
    }
}

/// USING registers the derived table's side through the names the derived table took from
/// its select list (Go `coalesceCommonColumns:1281-1297`). A plain column keeps its database
/// and original table (a non-LATERAL alias only renames the table, `buildResultSetNode:525-
/// 531`), so `t1.b` is registered again; a computed column has no database name and is
/// skipped (`:1291`), so only the base table's side is registered.
#[test]
fn using_registers_a_derived_table_through_its_names() {
    for (sql, want) in [
        (
            "SELECT 1 FROM (SELECT b FROM t1) d JOIN t2 USING (b)",
            vec![("t1", "b"), ("t1", "b"), ("t2", "b")],
        ),
        (
            "SELECT 1 FROM (SELECT b + 1 AS b FROM t1) d JOIN t2 USING (b)",
            vec![("t1", "b"), ("t2", "b")],
        ),
    ] {
        let vi: Vec<VisitInfo> = with_plan(sql, |b, _, res| {
            res.unwrap_or_else(|e| panic!("build {sql}: {e}"));
            b.get_visit_info().to_vec()
        });
        let cols: Vec<(&str, &str)> = vi
            .iter()
            .filter(|v| v.column != "*")
            .map(|v| (v.table.as_str(), v.column.as_str()))
            .collect();
        // The first t1.b comes from the derived table's own select list.
        assert_eq!(cols, want, "{sql}: {vi:?}");
    }
}

/// `TableIsView` (Go `infoschema.go`): a view is a view, a table or a missing name is not.
#[test]
fn table_is_view_answers_from_the_infoschema() {
    let is = tables();
    let test = CIStr::new("test");
    assert!(table_is_view(&*is, &test, &CIStr::new("v1")).unwrap());
    assert!(!table_is_view(&*is, &test, &CIStr::new("t1")).unwrap());
    assert!(!table_is_view(&*is, &test, &CIStr::new("nosuch")).unwrap());
    assert!(!table_is_view(&*is, &CIStr::new("other"), &CIStr::new("v1")).unwrap());
}

/// USING looks the side's table name (the alias, if any) up with `TableIsView`
/// (`coalesceCommonColumns:1287-1290`): for a table it registers the original table's
/// column; when the alias is a view's name, Go registers the view's column instead. The
/// second case is Go v8.5.8's behaviour, kept as is and reported in #548.
#[test]
fn using_registers_through_table_is_view() {
    for (sql, want) in [
        ("SELECT 1 FROM t1 AS x JOIN t2 USING (b)", ("t1", "b")),
        ("SELECT 1 FROM t1 AS v1 JOIN t2 USING (b)", ("v1", "b")),
    ] {
        let vi: Vec<VisitInfo> = with_plan(sql, |b, _, res| {
            res.unwrap_or_else(|e| panic!("build {sql}: {e}"));
            b.get_visit_info().to_vec()
        });
        let cols: Vec<(&str, &str)> = vi
            .iter()
            .filter(|v| v.column != "*")
            .map(|v| (v.table.as_str(), v.column.as_str()))
            .collect();
        assert_eq!(cols, vec![want, ("t2", "b")], "{sql}: {vi:?}");
    }
}
