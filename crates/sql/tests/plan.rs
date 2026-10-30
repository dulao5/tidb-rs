//! Plan shapes, name resolution errors and result metadata for an in-memory sysbench schema.

use tidb_meta::InfoSchema;
use tidb_meta::model::CIStr;
use tidb_meta::model::ColumnInfo;
use tidb_meta::model::DbInfo;
use tidb_meta::model::FieldType;
use tidb_meta::model::SchemaState;
use tidb_meta::model::TableInfo;
use tidb_meta::model::flag;
use tidb_meta::model::tp;
use tidb_mysql_protocol::ColumnType;
use tidb_mysql_protocol::SqlError;
use tidb_sql::Plan;
use tidb_sql::PlanContext;
use tidb_sql::Statement;
use tidb_sql::ir::Literal;
use tidb_sql::parse_and_lower;
use tidb_sql::plan::Source;
use tidb_sql::plan_select;

fn name(s: &str) -> CIStr {
    CIStr {
        o: s.to_owned(),
        l: s.to_lowercase(),
    }
}

fn int_ft(t: u8, flen: i64, flags: u32) -> FieldType {
    FieldType {
        tp: t,
        flag: flags,
        flen,
        decimal: 0,
        charset: "binary".into(),
        collate: "binary".into(),
        ..FieldType::default()
    }
}

fn char_ft(flen: i64) -> FieldType {
    FieldType {
        tp: tp::STRING,
        flag: flag::NOT_NULL,
        flen,
        decimal: 0,
        charset: "utf8mb4".into(),
        collate: "utf8mb4_bin".into(),
        ..FieldType::default()
    }
}

fn col(id: i64, n: &str, offset: usize, ft: FieldType) -> ColumnInfo {
    ColumnInfo {
        id,
        name: name(n),
        offset,
        field_type: ft,
        state: SchemaState::Public,
        ..ColumnInfo::default()
    }
}

/// sysbench's sbtest1 as Go TiDB creates it, plus a few odd tables.
fn schema() -> InfoSchema {
    let sbtest1 = TableInfo {
        id: 114,
        name: name("sbtest1"),
        charset: "utf8mb4".into(),
        collate: "utf8mb4_bin".into(),
        columns: vec![
            col(
                1,
                "id",
                0,
                int_ft(
                    tp::LONG,
                    11,
                    flag::NOT_NULL | flag::PRI_KEY | flag::AUTO_INCREMENT,
                ),
            ),
            col(
                2,
                "k",
                1,
                int_ft(tp::LONG, 11, flag::NOT_NULL | flag::MULTIPLE_KEY),
            ),
            col(3, "c", 2, char_ft(120)),
            col(4, "pad", 3, char_ft(60)),
        ],
        state: SchemaState::Public,
        pk_is_handle: true,
        ..TableInfo::default()
    };
    let mut unsigned = sbtest1.clone();
    unsigned.id = 115;
    unsigned.name = name("upk");
    unsigned.columns[0].field_type.flag |= flag::UNSIGNED;
    let mut rowid = sbtest1.clone();
    rowid.id = 116;
    rowid.name = name("norowid");
    rowid.pk_is_handle = false;
    rowid.columns[0].field_type.flag &= !flag::PRI_KEY;
    let mut ts = sbtest1.clone();
    ts.id = 117;
    ts.name = name("withts");
    ts.columns
        .push(col(5, "ts", 4, int_ft(tp::TIMESTAMP, 19, 0)));
    InfoSchema::new(
        1,
        vec![(
            DbInfo {
                id: 1,
                name: name("sbtest"),
                state: SchemaState::Public,
                ..DbInfo::default()
            },
            vec![sbtest1, unsigned, rowid, ts],
        )],
    )
}

fn plan_with(sql: &str, params: &[Literal], describe: bool) -> Result<Plan, SqlError> {
    let lowered = parse_and_lower(sql)?;
    let Statement::Select(sel) = lowered.stmt else {
        panic!("not a select: {sql}");
    };
    let is = schema();
    plan_select(
        &sel,
        &is,
        PlanContext {
            current_db: Some("sbtest"),
            params,
            describe_only: describe,
        },
    )
}

fn plan(sql: &str) -> Plan {
    plan_with(sql, &[], false).unwrap_or_else(|e| panic!("{sql}: {e}"))
}

fn explain(sql: &str) -> String {
    plan(sql).to_string()
}

fn err(sql: &str) -> SqlError {
    plan_with(sql, &[], false).expect_err(sql)
}

#[test]
fn sysbench_point_select() {
    assert_eq!(
        explain("SELECT c FROM sbtest1 WHERE id=7"),
        "Projection [c]\n└─PointGet sbtest1 handle:7 columns:[c]\n"
    );
    let p = plan_with(
        "SELECT c FROM sbtest1 WHERE id=?",
        &[Literal::Int(42)],
        false,
    )
    .unwrap();
    assert_eq!(p.source, Source::PointGet { handle: 42 });
    let c = &p.columns[0];
    assert_eq!(
        (
            c.schema.as_str(),
            c.table.as_str(),
            c.org_table.as_str(),
            c.name.as_str(),
            c.org_name.as_str()
        ),
        ("sbtest", "sbtest1", "sbtest1", "c", "c")
    );
    assert_eq!(c.column_type, ColumnType::String);
    assert_eq!(c.column_length, 480);
    assert_eq!(c.charset, 46);
    assert_eq!(c.flags, flag::NOT_NULL as u16);
}

#[test]
fn sysbench_simple_range() {
    assert_eq!(
        explain("SELECT c FROM sbtest1 WHERE id BETWEEN 5018 AND 5117"),
        "Projection [c]\n\
         └─TableReader keep_order:true\n\
         \x20 └─TableRangeScan sbtest1 range:[5018,5117] columns:[c]\n"
    );
}

#[test]
fn sysbench_sum_range() {
    let p = plan("SELECT SUM(k) FROM sbtest1 WHERE id BETWEEN 5018 AND 5117");
    assert_eq!(
        p.to_string(),
        "Projection [sum(#0)]\n\
         └─HashAgg(final) funcs:[sum(#0)] group_by:[]\n\
         \x20 └─TableReader keep_order:false\n\
         \x20   └─HashAgg(cop partial) funcs:[sum(k)] group_by:[]\n\
         \x20     └─TableRangeScan sbtest1 range:[5018,5117] columns:[k]\n"
    );
    let c = &p.columns[0];
    assert_eq!(c.name, "SUM(k)");
    assert_eq!(c.org_name, "");
    assert_eq!(c.table, "");
    assert_eq!(c.schema, "");
    assert_eq!(c.column_type, ColumnType::NewDecimal);
    // DECIMAL(32,0): 11 + 21, plus one for the sign.
    assert_eq!(c.column_length, 33);
    assert_eq!(c.decimals, 0);
    assert_eq!(c.charset, 63);
    assert_eq!(c.flags, flag::BINARY as u16);
}

#[test]
fn sysbench_order_range() {
    assert_eq!(
        explain("SELECT c FROM sbtest1 WHERE id BETWEEN 5018 AND 5117 ORDER BY c"),
        "Projection [c]\n\
         └─Sort [c]\n\
         \x20 └─TableReader keep_order:true\n\
         \x20   └─TableRangeScan sbtest1 range:[5018,5117] columns:[c]\n"
    );
}

#[test]
fn sysbench_distinct_range() {
    let p = plan("SELECT DISTINCT c FROM sbtest1 WHERE id BETWEEN 5018 AND 5117 ORDER BY c");
    assert_eq!(
        p.to_string(),
        "Projection [firstrow(#0)]\n\
         └─Sort [firstrow(#0)]\n\
         \x20 └─HashAgg(final) funcs:[firstrow(#0)] group_by:[#1]\n\
         \x20   └─TableReader keep_order:false\n\
         \x20     └─HashAgg(cop partial) funcs:[firstrow(c)] group_by:[c]\n\
         \x20       └─TableRangeScan sbtest1 range:[5018,5117] columns:[c]\n"
    );
    let c = &p.columns[0];
    assert_eq!((c.table.as_str(), c.org_name.as_str()), ("sbtest1", "c"));
    assert_eq!(c.column_length, 480);
}

#[test]
fn prepared_statements_describe_without_params() {
    for sql in [
        "SELECT c FROM sbtest1 WHERE id=?",
        "SELECT c FROM sbtest1 WHERE id BETWEEN ? AND ?",
        "SELECT SUM(k) FROM sbtest1 WHERE id BETWEEN ? AND ?",
        "SELECT c FROM sbtest1 WHERE id BETWEEN ? AND ? ORDER BY c",
        "SELECT DISTINCT c FROM sbtest1 WHERE id BETWEEN ? AND ? ORDER BY c",
        "SELECT c FROM sbtest1 WHERE id > ? LIMIT ?",
    ] {
        let described = plan_with(sql, &[], true).unwrap();
        assert_eq!(described.source, Source::Dual, "{sql}");
        let executed = plan_with(sql, &[Literal::Int(1), Literal::Int(5)], false).unwrap();
        assert_eq!(described.columns, executed.columns, "{sql}");
    }
}

#[test]
fn ranges_and_edge_cases() {
    let src = |sql: &str| plan(sql).source;
    let reader = |lo, hi| Source::TableReader {
        lo,
        hi,
        desc: false,
        keep_order: true,
        limit: None,
        agg: None,
    };
    assert_eq!(
        src("SELECT c FROM sbtest1 WHERE id BETWEEN 10 AND 5"),
        Source::Dual
    );
    assert_eq!(
        src("SELECT c FROM sbtest1 WHERE id BETWEEN 5 AND 5"),
        Source::PointGet { handle: 5 }
    );
    assert_eq!(
        src("SELECT c FROM sbtest1 WHERE id > 3 AND id <= 9"),
        reader(4, 9)
    );
    assert_eq!(
        src("SELECT c FROM sbtest1 WHERE 3 < id AND 9 >= id"),
        reader(4, 9)
    );
    assert_eq!(
        src("SELECT c FROM sbtest1 WHERE id < 5.5 AND id >= -2.5"),
        reader(-2, 5)
    );
    assert_eq!(src("SELECT c FROM sbtest1 WHERE id = 5.5"), Source::Dual);
    assert_eq!(
        src("SELECT c FROM sbtest1 WHERE id = '6'"),
        Source::PointGet { handle: 6 }
    );
    assert_eq!(src("SELECT c FROM sbtest1 WHERE id = NULL"), Source::Dual);
    assert_eq!(
        src("SELECT c FROM sbtest1 WHERE id BETWEEN NULL AND 5"),
        Source::Dual
    );
    assert_eq!(
        src("SELECT c FROM sbtest1 WHERE id = 1 AND id = 2"),
        Source::Dual
    );
    assert_eq!(
        src("SELECT c FROM sbtest1 WHERE id = -9223372036854775808"),
        Source::PointGet { handle: i64::MIN }
    );
    assert_eq!(
        src("SELECT c FROM sbtest1 WHERE id = 9223372036854775808"),
        Source::Dual
    );
    assert_eq!(
        src(
            "SELECT c FROM sbtest1 WHERE id BETWEEN -99999999999999999999 AND 99999999999999999999"
        ),
        reader(i64::MIN, i64::MAX)
    );
    assert_eq!(src("SELECT c FROM sbtest1"), reader(i64::MIN, i64::MAX));
    // Beyond INT's range: no row can match, Go's fast path makes it a TableDual.
    assert_eq!(
        src("SELECT c FROM sbtest1 WHERE id = 3000000000"),
        Source::Dual
    );
    // Prepared parameters, including numeric strings.
    let p = plan_with(
        "SELECT c FROM sbtest1 WHERE id BETWEEN ? AND ?",
        &[Literal::Str("10".into()), Literal::Float(20.5)],
        false,
    )
    .unwrap();
    assert_eq!(p.source, reader(10, 20));
    let e = plan_with(
        "SELECT c FROM sbtest1 WHERE id = ?",
        &[Literal::Str("abc".into())],
        false,
    )
    .unwrap_err();
    assert_eq!(e.code, 1235);
}

#[test]
fn limit_and_handle_order() {
    assert_eq!(
        explain("SELECT id, c FROM sbtest1 WHERE id > 100 ORDER BY id DESC LIMIT 2, 3"),
        "Projection [id, c]\n\
         └─Limit offset:2 count:3\n\
         \x20 └─TableReader keep_order:true\n\
         \x20   └─Limit(cop) 5\n\
         \x20     └─TableRangeScan sbtest1 range:[101,9223372036854775807] desc columns:[id, c]\n"
    );
    assert_eq!(
        explain("SELECT c FROM sbtest1 ORDER BY k DESC, c LIMIT 1"),
        "Projection [c]\n\
         └─Limit offset:0 count:1\n\
         \x20 └─Sort [k:desc, c]\n\
         \x20   └─TableReader keep_order:true\n\
         \x20     └─TableRangeScan sbtest1 range:[-9223372036854775808,9223372036854775807] columns:[c, k]\n"
    );
    let p = plan_with("SELECT c FROM sbtest1 LIMIT ?", &[Literal::Int(4)], false).unwrap();
    assert_eq!(p.limit.unwrap().count, 4);
    // LIMIT 0 reads nothing (Go: TableDual); a pushed Limit 0 would crash TiKV.
    for sql in [
        "SELECT c FROM sbtest1 LIMIT 0",
        "SELECT c FROM sbtest1 LIMIT 5, 0",
        "SELECT c FROM sbtest1 WHERE id = 3 LIMIT 0",
        "SELECT c FROM sbtest1 ORDER BY k LIMIT 0",
        "SELECT count(*) FROM sbtest1 LIMIT 0",
    ] {
        assert!(matches!(plan(sql).source, Source::Dual), "{sql}");
    }
    let p = plan_with("SELECT c FROM sbtest1 LIMIT ?", &[Literal::Int(0)], false).unwrap();
    assert!(matches!(p.source, Source::Dual));
    // Counts above MaxInt64 are uint64 (Go getUint64FromNUM), not 0.
    for n in [9223372036854775808u64, u64::MAX] {
        let p = plan(&format!("SELECT c FROM sbtest1 LIMIT {n}"));
        assert_eq!(p.limit.unwrap().count, n);
        assert!(
            matches!(p.source, Source::TableReader { limit: Some(l), .. } if l == n),
            "{:?}",
            p.source
        );
    }
    assert_eq!(
        plan_with("SELECT c FROM sbtest1 LIMIT ?", &[Literal::Int(-1)], false)
            .unwrap_err()
            .code,
        1210
    );
}

#[test]
fn aggregates_and_group_by() {
    assert_eq!(
        explain(
            "SELECT k, COUNT(*), MAX(c) FROM sbtest1 WHERE id < 100 GROUP BY k ORDER BY 2 DESC"
        ),
        "Projection [firstrow(#2), count(#0), max(#1)]\n\
         └─Sort [count(#0):desc]\n\
         \x20 └─HashAgg(final) funcs:[firstrow(#0), count(#1), max(#2)] group_by:[#3]\n\
         \x20   └─TableReader keep_order:false\n\
         \x20     └─HashAgg(cop partial) funcs:[firstrow(k), count(id), max(c)] group_by:[k]\n\
         \x20       └─TableRangeScan sbtest1 range:[-9223372036854775808,99] columns:[k, id, c]\n"
            .replace(
                "firstrow(#2), count(#0), max(#1)",
                "firstrow(#0), count(#1), max(#2)"
            )
            .replace("Sort [count(#0):desc]", "Sort [count(#1):desc]")
    );
    // Aggregation over a point get runs completely at the root.
    assert_eq!(
        explain("SELECT SUM(k), COUNT(1) FROM sbtest1 WHERE id = 3"),
        "Projection [sum(k), count(id)]\n\
         └─HashAgg(complete) funcs:[sum(k), count(id)] group_by:[]\n\
         \x20 └─PointGet sbtest1 handle:3 columns:[k, id]\n"
    );
    let p = plan("SELECT COUNT(*) AS n FROM sbtest1");
    assert_eq!(p.columns[0].name, "n");
    assert_eq!(p.columns[0].column_type, ColumnType::LongLong);
    assert_eq!(p.columns[0].column_length, 21);
    assert_eq!(p.columns[0].flags, (flag::NOT_NULL | flag::BINARY) as u16);
    let p = plan("SELECT MIN(c) FROM sbtest1 WHERE id BETWEEN 1 AND 9");
    // MIN keeps the argument type without NOT NULL.
    assert_eq!(p.columns[0].flags, 0);
    assert_eq!(p.columns[0].column_type, ColumnType::String);
}

#[test]
fn names_aliases_and_wildcards() {
    let p = plan("SELECT t.ID AS x, K, t.* FROM SBTEST1 t WHERE id BETWEEN 1 AND 2");
    let names: Vec<_> = p
        .columns
        .iter()
        .map(|c| {
            format!(
                "{}/{}/{}/{}/{}",
                c.schema, c.table, c.org_table, c.name, c.org_name
            )
        })
        .collect();
    assert_eq!(
        names,
        [
            "sbtest/t/sbtest1/x/id",
            "sbtest/t/sbtest1/K/k",
            "sbtest/t/sbtest1/id/id",
            "sbtest/t/sbtest1/k/k",
            "sbtest/t/sbtest1/c/c",
            "sbtest/t/sbtest1/pad/pad",
        ]
    );
    // Normal path: table name as stored.
    let p = plan("SELECT c FROM SBTEST1 WHERE id BETWEEN 1 AND 2");
    assert_eq!(p.columns[0].table, "sbtest1");
    // Go's PointGet fast path: table name as written (checked against v8.5.8 with tools/ab).
    let p = plan("SELECT c, sbtest1.* FROM SBTEST1 WHERE id = 1");
    assert_eq!(p.columns[0].table, "SBTEST1");
    assert_eq!(p.columns[0].org_name, "c");
    assert_eq!(p.columns[1].org_name, "id");
    assert_eq!(p.columns[1].name, "id");
    // ... but not at prepare time, which uses the normal planner.
    let p = plan_with("SELECT c FROM SBTEST1 WHERE id = 1", &[], true).unwrap();
    assert_eq!(p.columns[0].table, "sbtest1");
    // Qualified table name.
    let p = plan("SELECT sbtest.sbtest1.c FROM sbtest.sbtest1 WHERE id = 1");
    assert_eq!(p.columns[0].schema, "sbtest");
}

#[test]
fn resolution_errors() {
    let e = err("SELECT c FROM nope WHERE id = 1");
    assert_eq!(
        (e.code, e.message.as_str()),
        (1146, "Table 'sbtest.nope' doesn't exist")
    );
    let e = err("SELECT c FROM nodb.sbtest1");
    assert_eq!(
        (e.code, e.message.as_str()),
        (1146, "Table 'nodb.sbtest1' doesn't exist")
    );
    let e = err("SELECT x FROM sbtest1");
    assert_eq!(
        (e.code, e.message.as_str()),
        (1054, "Unknown column 'x' in 'field list'")
    );
    let e = err("SELECT c FROM sbtest1 WHERE t.id = 1");
    assert_eq!(
        (e.code, e.message.as_str()),
        (1054, "Unknown column 't.id' in 'where clause'")
    );
    let e = err("SELECT c FROM sbtest1 ORDER BY zz");
    assert_eq!(
        (e.code, e.message.as_str()),
        (1054, "Unknown column 'zz' in 'order clause'")
    );
    let e = err("SELECT c FROM sbtest1 ORDER BY 3");
    assert_eq!(e.code, 1054);
    let e = err("SELECT x.* FROM sbtest1");
    assert_eq!((e.code, e.message.as_str()), (1051, "Unknown table 'x'"));
    let lowered = parse_and_lower("SELECT c FROM sbtest1").unwrap();
    let Statement::Select(sel) = lowered.stmt else {
        panic!()
    };
    let e = plan_select(
        &sel,
        &schema(),
        PlanContext {
            current_db: None,
            params: &[],
            describe_only: false,
        },
    )
    .unwrap_err();
    assert_eq!((e.code, e.message.as_str()), (1046, "No database selected"));
}

#[test]
fn unsupported_is_an_error() {
    for sql in [
        "SELECT c FROM sbtest1 WHERE k = 5",
        "SELECT c FROM sbtest1 WHERE id = k",
        "SELECT c FROM upk WHERE id = 5",
        "SELECT c FROM norowid WHERE id = 5",
        "SELECT ts FROM withts",
        "SELECT c, SUM(k) FROM sbtest1",
        "SELECT SUM(c) FROM sbtest1",
        "SELECT DISTINCT k FROM sbtest1 GROUP BY k",
        "SELECT 1 FROM sbtest1",
        "SELECT k FROM sbtest1 GROUP BY k ORDER BY c",
    ] {
        assert_eq!(err(sql).code, 1235, "{sql}");
    }
    // Columns of other types in the same table are fine when not read.
    assert_eq!(
        plan("SELECT c FROM withts WHERE id = 1").source,
        Source::PointGet { handle: 1 }
    );
}
