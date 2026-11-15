//! Unit tests of the point get plan: the methods no govec record observes (the fastplan
//! domain's exemptions), the boundary stubs, and the handle columns of the write path.

use std::sync::Arc;

use tidb_base::Error;
use tidb_base::blocked::blocked_by;
use tidb_kv::key::{Handle, IntHandle};
use tidb_model::{PartitionDefinition, PartitionInfo, TableInfo};
use tidb_parser::model::CIStr;
use tidb_parser::{ParseOptions, parse};
use tidb_sessionctx::exprstatic as es;
use tidb_sessionctx::variable::session::{GlobalVarAccessor, SessionVars};
use tidb_types::datum::Datum;

use super::*;
use crate::core::resolve::new_node_w;

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

/// t(a BIGINT PRIMARY KEY, b INT, UNIQUE KEY ub(b)).
const TABLE: &str = r#"{"id":100,"name":{"O":"t","L":"t"},"charset":"utf8mb4","collate":"utf8mb4_bin","pk_is_handle":true,"state":5,"version":5,
"cols":[
 {"id":1,"name":{"O":"a","L":"a"},"offset":0,"type":{"Tp":8,"Flag":3,"Flen":20,"Decimal":0,"Charset":"binary","Collate":"binary"},"state":5,"version":2},
 {"id":2,"name":{"O":"b","L":"b"},"offset":1,"type":{"Tp":3,"Flag":4,"Flen":11,"Decimal":0,"Charset":"binary","Collate":"binary"},"state":5,"version":2}],
"index_info":[
 {"id":1,"idx_name":{"O":"ub","L":"ub"},"idx_cols":[{"name":{"O":"b","L":"b"},"offset":1,"length":-1}],"is_unique":true,"state":5}]}"#;

fn table() -> TableInfo {
    let mut t = TableInfo::default();
    t.unmarshal_json(TABLE.as_bytes()).unwrap();
    t
}

fn with_partitions(mut t: TableInfo) -> TableInfo {
    t.partition = Some(Box::new(PartitionInfo {
        enable: true,
        definitions: Some(vec![
            PartitionDefinition {
                id: 101,
                name: CIStr::new("p0"),
                ..Default::default()
            },
            PartitionDefinition {
                id: 102,
                name: CIStr::new("P1"),
                ..Default::default()
            },
        ]),
        ..Default::default()
    }));
    t
}

fn plan_of(t: TableInfo) -> PointGetPlan {
    let t = Arc::new(t);
    let (schema, names, _) =
        build_schema_from_fields(&CIStr::new("test"), &t, &t.name, &[]).unwrap();
    PointGetPlan {
        schema,
        output_names: names,
        tbl_info: t,
        ..PointGetPlan::default()
    }
}

#[test]
fn operator_info() {
    let mut p = plan_of(table());
    assert_eq!(p.operator_info(false, "OFF"), "");
    p.handle = Some(Handle::Int(IntHandle(-1)));
    assert_eq!(p.operator_info(false, "OFF"), "handle:-1");
    assert_eq!(p.operator_info(true, "OFF"), "handle:?");
    assert_eq!(p.operator_info(false, "ON"), "handle:?");
    assert_eq!(p.operator_info(false, "MARKER"), "handle:‹-1›");
    p.unsigned_handle = true;
    assert_eq!(p.operator_info(false, "OFF"), "handle:18446744073709551615");
    p.lock = true;
    assert_eq!(
        p.operator_info(false, "OFF"),
        "handle:18446744073709551615, lock"
    );
    p.handle = None;
    assert_eq!(p.operator_info(false, "OFF"), "lock");
}

#[test]
fn trivial_methods() {
    let mut p = plan_of(table());
    p.set_cost(2.5);
    assert_eq!(p.cost(), 2.5);
    assert!(p.attach2_task().is_none());
    assert!(p.to_pb().unwrap().is_none());
    assert!(p.children().is_empty());
    p.set_children(Vec::new());
    assert!(p.get_child_req_props(0).is_none());
    assert!(p.extract_correlated_cols().is_empty());
    assert_eq!(p.stats_count(), 1.0);
    assert_eq!(
        p.clone_plan().err().unwrap().to_string(),
        "*core.PointGetPlan doesn't support cloning"
    );
    p.resolve_indices().unwrap();
    assert_eq!(
        blocked_by(&p.load_table_stats().unwrap_err()),
        Some("D10.2")
    );
    assert_eq!(
        blocked_by(&p.get_avg_row_size().unwrap_err()),
        Some("D10.5")
    );
    assert_eq!(
        blocked_by(&p.explain_info_checked().unwrap_err()),
        Some("D10.9")
    );
}

impl PointGetPlan {
    /// `explain_info` panics with its boundary stub error (the trait returns a String).
    fn explain_info_checked(&self) -> Result<String, Error> {
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.explain_info()));
        r.map_err(|e| {
            let msg = e.downcast_ref::<String>().cloned().unwrap_or_default();
            assert!(msg.contains("D10.9"), "{msg}");
            tidb_base::blocked::blocked("D10.9", "explain")
        })
    }
}

#[test]
fn prune_partitions_and_explicit_partitions() {
    // No partitions: nothing to prune.
    let mut p = plan_of(table());
    assert!(!p.prune_partitions(false).unwrap());

    // _tidb_rowid + one explicit partition (a table without a clustered index).
    let mut t = with_partitions(table());
    t.pk_is_handle = false;
    let mut p = plan_of(t);
    p.partition_names = vec![CIStr::new("p1")];
    assert!(!p.prune_partitions(false).unwrap());
    assert_eq!(p.partition_idx, Some(1));

    // A global index reads the table ID.
    let t = with_partitions(table());
    let mut p = plan_of(t.clone());
    let mut idx = (*t.indices.as_ref().unwrap()[0]).clone();
    idx.global = true;
    p.index_info = Some(Arc::new(idx));
    assert!(!p.prune_partitions(false).unwrap());

    // Pruning by value: the partitioned table (D06.4 part 2).
    let mut p = plan_of(t.clone());
    assert_eq!(
        blocked_by(&p.prune_partitions(false).unwrap_err()),
        Some("D06.4")
    );
    p.partition_idx = Some(0);
    assert!(!p.prune_partitions(false).unwrap());
    assert_eq!(
        blocked_by(&p.prune_partitions(true).unwrap_err()),
        Some("D06.4")
    );
    // LoadTableStats of no matching partition loads nothing.
    p.partition_idx = Some(-1);
    p.load_table_stats().unwrap();

    let pi = t.get_partition_info();
    assert!(is_in_explicit_partitions(pi, 1, &[]));
    assert!(is_in_explicit_partitions(pi, 1, &[CIStr::new("p1")]));
    assert!(!is_in_explicit_partitions(pi, 0, &[CIStr::new("p1")]));
    assert!(!is_in_explicit_partitions(pi, -1, &[CIStr::new("p0")]));
    assert!(!is_in_explicit_partitions(pi, 2, &[CIStr::new("p0")]));
    assert!(!is_in_explicit_partitions(None, 0, &[CIStr::new("p0")]));
    assert_eq!(
        blocked_by(&get_hash_or_key_partition_column_name(&t).unwrap_err()),
        Some("D06.4")
    );
    assert_eq!(
        get_hash_or_key_partition_column_name(&table()).unwrap(),
        None
    );
}

#[test]
fn handle_cols_of_the_write_path() {
    // Int handle: the schema's column.
    let t = Arc::new(table());
    let mut p = plan_of(table());
    assert!(matches!(
        build_handle_cols("test", &t, &mut p),
        HandleCols::Int(_)
    ));
    assert_eq!(p.schema.len(), 2);

    // _tidb_rowid: appended to the schema and the output names.
    let mut t2 = table();
    t2.pk_is_handle = false;
    let t2 = Arc::new(t2);
    let mut p = plan_of((*t2).clone());
    let HandleCols::Int(_) = build_handle_cols("db", &t2, &mut p) else {
        panic!("int handle cols");
    };
    assert_eq!(p.schema.columns[2].id, tidb_model::ExtraHandleID as i64);
    assert_eq!(p.schema.columns[2].index, 2);
    assert_eq!(p.schema.len(), 3);
    let last = p.output_names.last().unwrap();
    assert_eq!(last.col_name.l, "_tidb_rowid");
    assert_eq!(last.db_name.o, "db");
    assert_eq!(last.tbl_name.l, "t");
}

#[test]
fn point_get_value() {
    let t = table();
    let col = &t.columns.as_ref().unwrap()[1];
    let sc = tidb_sessionctx::stmtctx::new_stmt_ctx();
    let mut d = Datum::default();
    d.set_string(b"12".to_vec(), "utf8mb4_bin");
    assert_eq!(
        get_point_get_value(&sc, col, &mut d).unwrap().get_int64(),
        12
    );
    d.set_string(b"1x".to_vec(), "utf8mb4_bin");
    assert!(get_point_get_value(&sc, col, &mut d).is_none());
    let mut d = Datum::default();
    d.set_float64(1.5);
    assert!(get_point_get_value(&sc, col, &mut d).is_none());
}

/// The statements that stop at a boundary stub before any vector could compare them.
#[test]
fn try_fast_plan_write_path_and_subqueries() {
    let mut vars = SessionVars::new(Box::new(Acc));
    vars.current_db = "test".to_owned();
    let expr_ctx: tidb_sessionctx::exprctx::ExprContextRef = es::new_expr_context(Vec::new());
    for (sql, block) in [
        ("update t set b = 1 where a = 1", Some("X")),
        ("delete from t where a = 1", Some("X")),
        ("select 1", None),
    ] {
        let mut stmt = parse(sql, &ParseOptions::default())
            .unwrap()
            .stmts
            .into_iter()
            .next()
            .unwrap();
        let node = new_node_w(&mut stmt);
        let mut ctx = PlanCtx::new(&mut vars, Arc::clone(&expr_ctx));
        let res = try_fast_plan(&mut ctx, &node);
        match block {
            Some(b) => assert_eq!(blocked_by(&res.err().unwrap()), Some(b), "{sql}"),
            None => assert!(res.unwrap().is_none(), "{sql}"),
        }
    }
    let e = Expr::default();
    assert_eq!(
        blocked_by(&is_expr_has_sub_query(&e).unwrap_err()),
        Some("X")
    );
}

/// `IsPointGetWithPKOrUniqueKeyByAutoCommit` on a point get (the arm VS1-11 ports).
#[test]
fn point_get_by_auto_commit() {
    use crate::core::common_plans::is_point_get_with_pk_or_unique_key_by_auto_commit as f;
    let mut vars = SessionVars::new(Box::new(Acc));
    let t = table();
    let mut p = plan_of(t.clone());
    p.handle = Some(Handle::Int(IntHandle(1)));
    assert!(f(&vars, &p).unwrap());
    // A unique index needs a second read.
    p.index_info = Some(Arc::clone(&t.indices.as_ref().unwrap()[0]));
    assert!(!f(&vars, &p).unwrap());
    // The primary key of a clustered table does not.
    let mut idx = (*t.indices.as_ref().unwrap()[0]).clone();
    idx.primary = true;
    let mut ct = t.clone();
    ct.is_common_handle = true;
    let mut p = plan_of(ct.clone());
    p.index_info = Some(Arc::new(idx));
    assert!(f(&vars, &p).unwrap());
    // A cached table.
    ct.table_cache_status_type = tidb_model::TableCacheStatusEnable;
    let mut p2 = plan_of(ct);
    p2.index_info = p.index_info.clone();
    assert!(!f(&vars, &p2).unwrap());
    // Not autocommit.
    vars.set_status_flag(tidb_base::mysql::ServerStatusAutocommit, false);
    assert!(!f(&vars, &p).unwrap());
}
