//! Statements over the sysbench table run through the session (spec
//! `specs/D12.2-basic-executors/spec.md` §6, the regressions of the first port, issue #328):
//! the rows and the result columns of each statement that runs on the new stack are what Go
//! v8.5.8 returns, and a statement that cannot run yet falls back before any executor opened.
//!
//! The table is `test.sbtest1` as Go v8.5.8 stores it after
//! `CREATE TABLE sbtest1 (id INT NOT NULL AUTO_INCREMENT, k INT NOT NULL DEFAULT '0',
//! c CHAR(120) NOT NULL DEFAULT '', pad CHAR(60) NOT NULL DEFAULT '', PRIMARY KEY (id),
//! KEY k_1 (k))` (`sbtest1_table_info.json`: the `TableInfo` a v8.5.8 server returned on its
//! status port, `/schema/test/sbtest1`, with the update timestamp cleared), holding the rows
//! `id` 1..=100, `k` = 10 × `id`, `c` = "c<id>", `pad` = "p<id>".
//!
//! The expected columns are the column definitions the same v8.5.8 server (on unistore, the
//! same table and rows) sent for each statement, text and prepared (`COM_STMT_PREPARE` and
//! `COM_STMT_EXECUTE`). The session's result fields are checked here; the wire definition is
//! `ConvertColumnInfo` of them (`crates/server`): the length of a string column is its
//! `flen` times 4 (utf8mb4), of a decimal `flen` + 1 (+ 1 more with decimals), of another
//! type `flen`; an unspecified `flen` is the type's default length, and an unspecified
//! decimal is 31 (`NotFixedDec`).

use super::*;

use tidb_types::datum::{Datum, new_int_datum, new_string_datum};

/// `TableInfo` of `test.sbtest1` (see the module doc).
const SBTEST1_TABLE_INFO: &str = include_str!("sbtest1_table_info.json");

/// The id of the database `test` (the InfoSchema loader sets it on the table).
const TEST_DB_ID: i64 = 2;

/// The sysbench table's `TableInfo`.
fn sbtest1_table_info() -> tidb_model::TableInfo {
    let mut t = tidb_model::TableInfo::default();
    t.unmarshal_json(SBTEST1_TABLE_INFO.as_bytes()).unwrap();
    t.dbid = TEST_DB_ID;
    t
}

/// An InfoSchema with `test.sbtest1` of [`SBTEST1_TABLE_INFO`].
fn sysbench_info_schema() -> Arc<tidb_infoschema::InfoSchemaV2> {
    use tidb_model::{DBInfo, StatePublic};
    let mut db = DBInfo {
        id: TEST_DB_ID,
        name: tidb_parser::model::CIStr::new("test"),
        state: StatePublic,
        ..Default::default()
    };
    db.deprecated.tables = Some(vec![Arc::new(sbtest1_table_info())]);
    let mut b = tidb_infoschema::new_builder(None, None, tidb_infoschema::new_data());
    block_on(b.init_with_db_infos(vec![db], vec![], vec![], 1)).unwrap();
    Arc::new(block_on(b.build(1)))
}

/// Puts the row `(id, 10 × id, "c<id>", "p<id>")` into the mock store.
fn put_sbtest1_row(store: &MockStore, table_id: i64, id: i64) {
    let mut enc = tidb_tablecodec::rowcodec::Encoder::default();
    enc.enable = true;
    let mut c = Datum::default();
    c.set_string(format!("c{id}").into_bytes(), "utf8mb4_bin".to_owned());
    let mut pad = Datum::default();
    pad.set_string(format!("p{id}").into_bytes(), "utf8mb4_bin".to_owned());
    let row = [new_int_datum(id.wrapping_mul(10)), c, pad];
    let val = tidb_tablecodec::encode_row(None, &row, &[2, 3, 4], None, Some(&mut enc)).unwrap();
    let key = tidb_tablecodec::encode_row_key_with_handle(
        table_id,
        &tidb_kv::Handle::Int(tidb_kv::key::IntHandle(id)),
    );
    store.rows.lock().insert(key.0, val);
}

/// A session over a mock store with the sysbench table and its 100 rows; the prepared plan
/// cache is off.
fn sysbench_session() -> (Arc<MockStore>, Session) {
    let store = MockStore::new(10);
    let table_id = sbtest1_table_info().id;
    for id in 1..=100 {
        put_sbtest1_row(&store, table_id, id);
    }
    tidb_sessionctx::variable::tidb_vars::EnableMDL.store(true, SeqCst);
    let mut s = session(&store);
    s.set_domain_info_schema(sysbench_info_schema());
    s.vars_mut().current_db = "test".to_owned();
    (store, s)
}

/// [`sysbench_session`] with the prepared plan cache on (Go's default) over a fresh instance
/// cache, so that the second EXECUTE of a statement reuses the plan of the first.
fn sysbench_cached_session() -> (Arc<MockStore>, Session) {
    let (store, mut s) = sysbench_session();
    s.vars_mut().enable_prepared_plan_cache = true;
    s.set_instance_plan_cache(Some(new_test_cache()));
    (store, s)
}

/// A result column as the session describes it: name, original name (empty for an
/// expression), table alias, table, database; type, flen, decimal, flag and charset of its
/// field type.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Col {
    name: String,
    org_name: String,
    table: String,
    org_table: String,
    schema: String,
    tp: u8,
    flen: isize,
    decimal: isize,
    flag: u32,
    charset: String,
}

fn col_of(f: &tidb_executor::glue::ResultField) -> Col {
    let t = &f.column_type;
    Col {
        name: f.column_as_name.o.clone(),
        org_name: if f.empty_org_name {
            String::new()
        } else {
            f.column_name.o.clone()
        },
        table: f.table_as_name.o.clone(),
        org_table: f.table_name.o.clone(),
        schema: f.db_name.o.clone(),
        tp: t.get_type(),
        flen: t.get_flen(),
        decimal: t.get_decimal(),
        flag: t.get_flag() as u32,
        charset: t.get_charset().to_owned(),
    }
}

/// The rows of a record set as text (NULL as "NULL"), every row of every chunk.
fn rows_of(s: &mut Session, rs: &mut crate::ExecStmtResult) -> Vec<Vec<String>> {
    let fields = rs.fields(s).unwrap();
    let mut out = Vec::new();
    let mut chk = rs.new_chunk().unwrap();
    loop {
        block_on(rs.next(s, &mut chk)).unwrap();
        if chk.num_rows() == 0 {
            return out;
        }
        for i in 0..chk.num_rows() {
            let row = chk.get_row(i);
            let mut r = Vec::new();
            for (j, f) in fields.iter().enumerate() {
                let d = row.get_datum(j, &f.column_type);
                r.push(datum_text(&d));
            }
            out.push(r);
        }
    }
}

/// The column `c` as Go v8.5.8 describes it: `CHAR(120) NOT NULL`, utf8mb4 (wire: charset
/// 46, length 480, type 254, flags 1, decimals 0).
fn col_c() -> Col {
    Col {
        name: "c".to_owned(),
        org_name: "c".to_owned(),
        table: "sbtest1".to_owned(),
        org_table: "sbtest1".to_owned(),
        schema: "test".to_owned(),
        tp: tidb_base::mysql::TypeString,
        flen: 120,
        decimal: 0,
        flag: tidb_base::mysql::NotNullFlag as u32,
        charset: "utf8mb4".to_owned(),
    }
}

/// An expression column named `name` (no original name, table or database).
fn expr_col(name: &str, tp: u8, flen: isize, decimal: isize, flag: u32, charset: &str) -> Col {
    Col {
        name: name.to_owned(),
        org_name: String::new(),
        table: String::new(),
        org_table: String::new(),
        schema: String::new(),
        tp,
        flen,
        decimal,
        flag,
        charset: charset.to_owned(),
    }
}

/// An integer literal or integer arithmetic of `flen` digits: `BIGINT NOT NULL BINARY`
/// (wire: charset 63, length `flen`, type 8, flags 129, decimals 0).
fn int_col(name: &str, flen: isize) -> Col {
    use tidb_base::mysql::{BinaryFlag, NotNullFlag, TypeLonglong};
    expr_col(
        name,
        TypeLonglong,
        flen,
        0,
        (NotNullFlag | BinaryFlag) as u32,
        "binary",
    )
}

/// The result of a statement that ran: its columns and its rows.
type Ran = (Vec<Col>, Vec<Vec<String>>);

/// Runs a text query to the end.
fn run_text(s: &mut Session, sql: &str) -> Result<Ran, Error> {
    let mut rs = block_on(s.execute_stmt(parse(sql), sql))?.expect("a result set");
    let cols = rs.fields(s).unwrap().iter().map(col_of).collect();
    let rows = rows_of(s, &mut rs);
    block_on(rs.close(s)).unwrap();
    assert!(s.is_idle(), "{sql}: {}", s.state_snapshot());
    Ok((cols, rows))
}

/// Runs an EXECUTE of a prepared statement to the end.
fn run_prepared(s: &mut Session, stmt_id: u32, params: Vec<Datum>) -> Result<Ran, Error> {
    let mut rs = block_on(s.execute_prepared_stmt(stmt_id, params))?.expect("a result set");
    let cols = rs.fields(s).unwrap().iter().map(col_of).collect();
    let rows = rows_of(s, &mut rs);
    block_on(rs.close(s)).unwrap();
    assert!(s.is_idle(), "{}", s.state_snapshot());
    Ok((cols, rows))
}

fn texts(rows: &[&[&str]]) -> Vec<Vec<String>> {
    rows.iter()
        .map(|r| r.iter().map(|v| (*v).to_owned()).collect())
        .collect()
}

/// A projection over a point get reads the columns it projects (the panic of the first
/// port: `index out of range [0] with length 0` in `GoSlice::i64_at`, a dropped connection
/// on the text protocol and error 1105 on the binary one).
///
/// Go plans each of these as a projection over `Point_Get`, whose schema also holds `id`
/// (the column of the access condition) before `c`. The first port ran before the
/// projection's own `ResolveIndices` (D10.9, the column index resolution) was ported: the
/// projection's column references kept index 0 and read the integer column `id` as the
/// string `c`, whose offsets a fixed-length column does not have. Resolving the indices (now
/// on main) makes each reference point at its column; these statements pin it, also for
/// literals of every kind beside a column and for a range that is one point (`BETWEEN 5 AND
/// 5`, planned as a point get).
#[test]
fn projection_over_point_get_reads_its_columns() {
    use tidb_base::mysql::{BinaryFlag, NotNullFlag, TypeVarString, UnsignedFlag};
    let (_store, mut s) = sysbench_session();
    let u64_col = expr_col(
        "18446744073709551615",
        tidb_base::mysql::TypeLonglong,
        20,
        0,
        (NotNullFlag | BinaryFlag | UnsignedFlag) as u32,
        "binary",
    );
    // Go: charset 46, length 4 (1 × 4), type 253, flags 1, decimals 31.
    let x_col = expr_col("x", TypeVarString, 1, -1, NotNullFlag as u32, "utf8mb4");
    let cases: Vec<(&str, Ran)> = vec![
        (
            "SELECT c FROM sbtest1 WHERE id = 1",
            (vec![col_c()], texts(&[&["c1"]])),
        ),
        (
            "SELECT 1, c FROM sbtest1 WHERE id = 1",
            (vec![int_col("1", 1), col_c()], texts(&[&["1", "c1"]])),
        ),
        (
            "SELECT 18446744073709551615, c FROM sbtest1 WHERE id=1",
            (
                vec![u64_col, col_c()],
                texts(&[&["18446744073709551615", "c1"]]),
            ),
        ),
        (
            "SELECT 'x', c FROM sbtest1 WHERE id=1",
            (vec![x_col, col_c()], texts(&[&["x", "c1"]])),
        ),
        // Go: `1 + 1` style integer arithmetic, length 20.
        (
            "SELECT id + 1 FROM sbtest1 WHERE id = 1",
            (vec![int_col("id + 1", 20)], texts(&[&["2"]])),
        ),
        (
            "SELECT c FROM sbtest1 WHERE id BETWEEN 5 AND 5",
            (vec![col_c()], texts(&[&["c5"]])),
        ),
        (
            "SELECT c FROM sbtest1 WHERE id BETWEEN 0 AND 0",
            (vec![col_c()], vec![]),
        ),
        (
            "SELECT c FROM sbtest1 WHERE id='33'",
            (vec![col_c()], texts(&[&["c33"]])),
        ),
    ];
    for (sql, (cols, rows)) in cases {
        let (got_cols, got_rows) = run_text(&mut s, sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
        assert_eq!(got_cols, cols, "{sql}");
        assert_eq!(got_rows, rows, "{sql}");
    }
}

/// The prepared form of the projection over a point get, `SELECT ?, c FROM sbtest1 WHERE
/// id=?` (sysbench's shape), with the plan cache on: the first EXECUTE optimizes, the
/// second reuses the plan with other parameters, and both read their columns. The column of
/// `?` follows the parameter, as in Go: at PREPARE `VARCHAR` (charset 46, length 5, type 253,
/// flags 0, decimals 31), at EXECUTE an integer gives `BIGINT NOT NULL BINARY` (charset 63,
/// length 20, type 8, flags 129, decimals 0) and a string `VARCHAR NOT NULL` (charset 46,
/// length 5, type 253, flags 1, decimals 31).
#[test]
fn prepared_projection_over_point_get_reads_its_columns() {
    use tidb_base::mysql::{BinaryFlag, NotNullFlag, TypeLonglong, TypeVarString};
    let _serial = PREPARED_COUNT.lock();
    let (_store, mut s) = sysbench_cached_session();
    let sql = "SELECT ?, c FROM sbtest1 WHERE id=?";
    let info = block_on(s.prepare_stmt(sql)).unwrap();
    let prepared: Vec<Col> = info.fields.iter().map(col_of).collect();
    let param_col =
        |tp, flen, decimal, flag, charset| expr_col("?", tp, flen, decimal, flag, charset);
    assert_eq!(
        prepared,
        vec![param_col(TypeVarString, -1, -1, 0, "utf8mb4"), col_c()]
    );

    let int_param = param_col(
        TypeLonglong,
        -1,
        0,
        (NotNullFlag | BinaryFlag) as u32,
        "binary",
    );
    let str_param = param_col(TypeVarString, -1, -1, NotNullFlag as u32, "utf8mb4");
    let int = new_int_datum;
    let cases: Vec<(Vec<Datum>, Col, Vec<Vec<String>>)> = vec![
        (
            vec![int(7), int(1)],
            int_param.clone(),
            texts(&[&["7", "c1"]]),
        ),
        (vec![int(8), int(42)], int_param, texts(&[&["8", "c42"]])),
        (
            vec![new_string_datum("x"), int(3)],
            str_param,
            texts(&[&["x", "c3"]]),
        ),
        (vec![int(9), int(101)], int_col("?", -1), vec![]),
    ];
    for (params, col, rows) in cases {
        let what = format!("{sql} {params:?}");
        let (cols, got) =
            run_prepared(&mut s, info.stmt_id, params).unwrap_or_else(|e| panic!("{what}: {e}"));
        assert_eq!(cols, vec![col, col_c()], "{what}");
        assert_eq!(got, rows, "{what}");
    }
}

/// A projection whose input is estimated at a chunk or more runs in parallel (a fetcher and
/// workers on the runtime, `ProjectionExec.prepare`): the sysbench point select of
/// `id = -9223372036854775808` is estimated at a third of the pseudo table's rows, so its
/// projection over the point get is parallel here, and reads its column like the
/// unparallel one. The point get finds the one row stored at that id.
#[test]
fn parallel_projection_over_point_get_reads_its_columns() {
    let (store, mut s) = sysbench_session();
    put_sbtest1_row(&store, sbtest1_table_info().id, i64::MIN);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let sql = "SELECT c FROM sbtest1 WHERE id=-9223372036854775808";
    let (cols, rows) = rt.block_on(async {
        let mut rs = s
            .execute_stmt(parse(sql), sql)
            .await
            .unwrap()
            .expect("a result set");
        let fields = rs.fields(&s).unwrap();
        let mut rows = Vec::new();
        let mut chk = rs.new_chunk().unwrap();
        loop {
            rs.next(&mut s, &mut chk).await.unwrap();
            if chk.num_rows() == 0 {
                break;
            }
            for i in 0..chk.num_rows() {
                rows.push(vec![datum_text(
                    &chk.get_row(i).get_datum(0, &fields[0].column_type),
                )]);
            }
        }
        rs.close(&mut s).await.unwrap();
        (fields.iter().map(col_of).collect::<Vec<_>>(), rows)
    });
    assert_eq!(cols, vec![col_c()]);
    assert_eq!(rows, texts(&[&["c-9223372036854775808"]]));
    assert!(s.is_idle(), "{}", s.state_snapshot());
}

/// Statements without a table, and empty results over the table dual, with Go's rows and
/// column types: `SELECT 1` and `SELECT 1 LIMIT 1` (a projection over the dual, and a limit),
/// `LIMIT 0` and `WHERE 1 = 0` (no row), `BETWEEN 60 AND 50` (Go: a projection over
/// `TableDual`, no row). Go's wire lengths: `-42` 3, `1 + 1` 20, `1.5` 6 (`flen` 4, + 1 for
/// the sign and + 1 for the point).
#[test]
fn constant_and_empty_results() {
    use tidb_base::mysql::{BinaryFlag, NotNullFlag, TypeNewDecimal};
    let (_store, mut s) = sysbench_session();
    let one = || int_col("1", 1);
    let decimal = expr_col(
        "1.5",
        TypeNewDecimal,
        4,
        1,
        (NotNullFlag | BinaryFlag) as u32,
        "binary",
    );
    let cases: Vec<(&str, Ran)> = vec![
        ("SELECT 1", (vec![one()], texts(&[&["1"]]))),
        ("SELECT -42", (vec![int_col("-42", 3)], texts(&[&["-42"]]))),
        (
            "SELECT 1, 2, 3",
            (
                vec![one(), int_col("2", 1), int_col("3", 1)],
                texts(&[&["1", "2", "3"]]),
            ),
        ),
        (
            "SELECT 1 + 1",
            (vec![int_col("1 + 1", 20)], texts(&[&["2"]])),
        ),
        ("SELECT 1.5", (vec![decimal], texts(&[&["1.5"]]))),
        ("SELECT 1 LIMIT 1", (vec![one()], texts(&[&["1"]]))),
        ("SELECT 1 LIMIT 0", (vec![one()], vec![])),
        ("SELECT 1 FROM DUAL WHERE 1 = 0", (vec![one()], vec![])),
        (
            "SELECT c FROM sbtest1 WHERE id BETWEEN 60 AND 50",
            (vec![col_c()], vec![]),
        ),
    ];
    for (sql, (cols, rows)) in cases {
        let (got_cols, got_rows) = run_text(&mut s, sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
        assert_eq!(got_cols, cols, "{sql}");
        assert_eq!(got_rows, rows, "{sql}");
    }
}

/// Ranges over the primary key never return rows on the new stack yet: they need the table
/// reader (D12.1), so they fall back before any executor opened, as text queries and as
/// prepared statements, also when the plan cache holds the plan of an earlier EXECUTE with
/// other parameters. In the first port the prepared form returned 0 rows in strict mode for
/// `BETWEEN -100 AND 50` (Go: 50) and for the full `BIGINT` range (Go: 100): every parameter
/// marker had order 0 (issue #352), so `BETWEEN ? AND ?` read the first parameter twice and
/// was planned as a point get on it; the markers are numbered by position since #354, and
/// the range is planned as a range again. Go's rows for each range are the ids in it.
#[test]
fn ranges_fall_back_before_any_executor_opened() {
    let _serial = PREPARED_COUNT.lock();
    let (_store, mut s) = sysbench_cached_session();
    let ranges = [(1, 10), (-100, 50), (i64::MIN, i64::MAX), (5, 104), (1, 10)];
    for (lo, hi) in ranges {
        let sql = format!("SELECT c FROM sbtest1 WHERE id BETWEEN {lo} AND {hi}");
        let before = s.executed_stmts();
        let r = run_text(&mut s, &sql);
        assert_eq!(assert_falls_back(&s, &sql, before, r), "D12.1");
    }
    let sql = "SELECT c FROM sbtest1 WHERE id BETWEEN ? AND ?";
    let info = block_on(s.prepare_stmt(sql)).unwrap();
    assert_eq!(
        info.fields.iter().map(col_of).collect::<Vec<_>>(),
        vec![col_c()]
    );
    for (lo, hi) in ranges {
        let what = format!("{sql} ({lo}, {hi})");
        let before = s.executed_stmts();
        let r = run_prepared(
            &mut s,
            info.stmt_id,
            vec![new_int_datum(lo), new_int_datum(hi)],
        );
        assert_eq!(assert_falls_back(&s, &what, before, r), "D12.1");
    }
}

/// `SUM(k)` over a range (sysbench's sum range) needs the aggregation (D09.4) and falls back
/// at planning, before any executor opened, as a text query and as a prepared statement,
/// also for a range of one point, which Go plans as a stream aggregation over a projection
/// over a point get.
#[test]
fn sum_range_falls_back_at_planning() {
    let _serial = PREPARED_COUNT.lock();
    let (_store, mut s) = sysbench_cached_session();
    for sql in [
        "SELECT SUM(k) FROM sbtest1 WHERE id BETWEEN 1 AND 10",
        "SELECT SUM(k) FROM sbtest1 WHERE id BETWEEN 5 AND 5",
    ] {
        let before = s.executed_stmts();
        let r = run_text(&mut s, sql);
        assert_eq!(assert_falls_back(&s, sql, before, r), "D09.4");
    }
    let sql = "SELECT SUM(k) FROM sbtest1 WHERE id BETWEEN ? AND ?";
    let before = s.executed_stmts();
    let r = block_on(s.prepare_stmt(sql)).map(|info| info.stmt_id);
    if let Ok(stmt_id) = r {
        for (lo, hi) in [(1, 10), (5, 5)] {
            let what = format!("{sql} ({lo}, {hi})");
            let r = run_prepared(&mut s, stmt_id, vec![new_int_datum(lo), new_int_datum(hi)]);
            assert_eq!(assert_falls_back(&s, &what, before, r), "D09.4");
        }
    } else {
        assert_eq!(assert_falls_back(&s, sql, before, r), "D09.4");
    }
}

/// The stream aggregation executor (the aggregation executors D12.3, part A) is ported, but no
/// aggregate reaches it yet: the physical aggregation plans (D10.1) are not ported and every
/// statement with an aggregate, a GROUP BY or a DISTINCT falls back at planning (the
/// aggregation rules, D09.4), before any executor opened; also the shapes Go plans as a stream
/// aggregation (an aggregate over a point get or a range of the clustered index). The stop of
/// each statement is pinned, so the first statement that reaches the new executor fails here
/// and gets a row and column check instead.
#[test]
fn aggregates_fall_back_at_planning() {
    let _serial = PREPARED_COUNT.lock();
    let (_store, mut s) = sysbench_cached_session();
    for sql in [
        "SELECT COUNT(*) FROM sbtest1 WHERE id = 1",
        "SELECT MAX(k), MIN(k) FROM sbtest1 WHERE id BETWEEN 1 AND 10",
        "SELECT COUNT(k) FROM sbtest1 WHERE id BETWEEN 1 AND 10",
        "SELECT id, COUNT(*) FROM sbtest1 WHERE id BETWEEN 1 AND 10 GROUP BY id",
        "SELECT DISTINCT c FROM sbtest1 WHERE id BETWEEN 1 AND 10 ORDER BY c",
    ] {
        let before = s.executed_stmts();
        let r = run_text(&mut s, sql);
        assert_eq!(assert_falls_back(&s, sql, before, r), "D09.4", "{sql}");
    }
}

/// The name of an expression column is the text of its select field, as Go v8.5.8 sends it
/// (`buildProjectionFieldNameFromExpressions` takes `SelectField.Text()`), read here from
/// the result fields of the record sets these statements now produce: on the text protocol,
/// at PREPARE, and at EXECUTE when the plan cache misses and when it hits (asserted, as the
/// v8.5.8 server does: `@@last_plan_from_cache` 0, then 1). Before the statement text reached
/// the planner on every path (#361), the text protocol and EXECUTE gave empty names.
/// Leading whitespace and a comment are not part of the field's text
/// (`  /* x */ SELECT 1 + 1` is named `1 + 1` by the same server, on every path). The rows
/// are Go's too.
#[test]
fn record_sets_name_expression_columns_by_their_text() {
    let _serial = PREPARED_COUNT.lock();
    let (_store, mut s) = sysbench_cached_session();
    let names_of = |cols: &[Col]| cols.iter().map(|c| c.name.clone()).collect::<Vec<_>>();
    for (sql, names, rows) in [
        ("SELECT 1", vec!["1"], texts(&[&["1"]])),
        ("SELECT -42", vec!["-42"], texts(&[&["-42"]])),
        (
            "SELECT 1, 2, 3",
            vec!["1", "2", "3"],
            texts(&[&["1", "2", "3"]]),
        ),
        ("SELECT 1 + 1", vec!["1 + 1"], texts(&[&["2"]])),
        ("  /* x */ SELECT 1 + 1", vec!["1 + 1"], texts(&[&["2"]])),
        ("SELECT 1.5", vec!["1.5"], texts(&[&["1.5"]])),
        ("SELECT 1 LIMIT 1", vec!["1"], texts(&[&["1"]])),
        (
            "SELECT id + 1 FROM sbtest1 WHERE id = 1",
            vec!["id + 1"],
            texts(&[&["2"]]),
        ),
        (
            "SELECT 'x', c FROM sbtest1 WHERE id=1",
            vec!["x", "c"],
            texts(&[&["x", "c1"]]),
        ),
    ] {
        let (cols, got) = run_text(&mut s, sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
        assert_eq!(names_of(&cols), names, "text: {sql}");
        assert_eq!(got, rows, "text: {sql}");

        let info = block_on(s.prepare_stmt(sql)).unwrap();
        let prepared: Vec<Col> = info.fields.iter().map(col_of).collect();
        assert_eq!(names_of(&prepared), names, "PREPARE: {sql}");

        for (what, hit) in [("EXECUTE (miss)", false), ("EXECUTE (hit)", true)] {
            let (cols, got) = run_prepared(&mut s, info.stmt_id, vec![])
                .unwrap_or_else(|e| panic!("{what}: {sql}: {e}"));
            assert_eq!(s.vars().found_in_plan_cache, hit, "{what}: {sql}");
            assert_eq!(names_of(&cols), names, "{what}: {sql}");
            assert_eq!(
                cols, prepared,
                "{what}: the columns PREPARE described: {sql}"
            );
            assert_eq!(got, rows, "{what}: {sql}");
        }
        s.drop_prepared_stmt(info.stmt_id).unwrap();
        s.clean_closed_prepared_stmts();
    }
}

/// The column `id` as Go v8.5.8 describes it: `INT NOT NULL AUTO_INCREMENT PRIMARY KEY`
/// (wire: charset 63, length 11, type 3, flags 515, decimals 0).
fn col_id() -> Col {
    use tidb_base::mysql::{AutoIncrementFlag, NotNullFlag, PriKeyFlag, TypeLong};
    Col {
        name: "id".to_owned(),
        org_name: "id".to_owned(),
        table: "sbtest1".to_owned(),
        org_table: "sbtest1".to_owned(),
        schema: "test".to_owned(),
        tp: TypeLong,
        flen: 11,
        decimal: 0,
        flag: (NotNullFlag | PriKeyFlag | AutoIncrementFlag) as u32,
        charset: "binary".to_owned(),
    }
}

/// A multi-threaded runtime for the statements whose plans hold a sort: the sort executors
/// (D12.5) take the parallel path, whose workers are tasks spawned on the runtime the
/// statement runs on.
fn multi_thread_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
}

/// `ORDER BY` over the table dual and over a point get, and `ORDER BY ... LIMIT` (a top-n),
/// on the text protocol: with the parallel sort (#362) and the basic executors both on
/// main, these plans run to the end on the new stack. The rows and the columns are what a
/// Go v8.5.8 server (unistore, the same table and rows) returned for each statement; its
/// plans (`EXPLAIN`) are in the comments. Each input holds at most one row, so the order of
/// the rows is fixed and `ORDER BY RAND()` is checked by its rows and columns alone.
#[test]
fn sort_and_top_n_over_dual_and_point_get() {
    let rt = multi_thread_runtime();
    let _rt = rt.enter();
    let (_store, mut s) = sysbench_session();
    let cases: Vec<(&str, Ran)> = vec![
        // Sort > Projection > Point_Get.
        (
            "SELECT c FROM sbtest1 WHERE id = 1 ORDER BY c",
            (vec![col_c()], texts(&[&["c1"]])),
        ),
        // Projection > Sort > Projection > Projection > TableDual.
        (
            "SELECT 1 FROM DUAL ORDER BY RAND()",
            (vec![int_col("1", 1)], texts(&[&["1"]])),
        ),
        // The same plan, no row.
        (
            "SELECT 1 FROM DUAL WHERE 1 = 0 ORDER BY RAND()",
            (vec![int_col("1", 1)], vec![]),
        ),
        // Projection > Sort > Projection > Projection > Point_Get.
        (
            "SELECT c FROM sbtest1 WHERE id = 1 ORDER BY RAND()",
            (vec![col_c()], texts(&[&["c1"]])),
        ),
        // Projection > Projection > Sort > Projection > Projection > Point_Get.
        (
            "SELECT c, k + 1 FROM sbtest1 WHERE id = 7 ORDER BY k + 1",
            (vec![col_c(), int_col("k + 1", 20)], texts(&[&["c7", "71"]])),
        ),
        // Projection > TopN > Point_Get.
        (
            "SELECT c FROM sbtest1 WHERE id = 1 ORDER BY c LIMIT 1",
            (vec![col_c()], texts(&[&["c1"]])),
        ),
        // Projection > TopN > Projection > Projection > TableDual.
        (
            "SELECT 1 AS a FROM DUAL ORDER BY RAND() LIMIT 1",
            (vec![int_col("a", 1)], texts(&[&["1"]])),
        ),
        // Limit > Point_Get (the order is the handle's).
        (
            "SELECT id, c FROM sbtest1 WHERE id = 5 ORDER BY id DESC LIMIT 1",
            (vec![col_id(), col_c()], texts(&[&["5", "c5"]])),
        ),
        // TableDual.
        (
            "SELECT c FROM sbtest1 WHERE id = 1 ORDER BY c LIMIT 0",
            (vec![col_c()], vec![]),
        ),
    ];
    for (sql, (cols, rows)) in cases {
        let (got_cols, got_rows) = run_text(&mut s, sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
        assert_eq!(got_cols, cols, "{sql}");
        assert_eq!(got_rows, rows, "{sql}");
    }
}

/// The prepared forms, with the plan cache on: each EXECUTE's columns, rows and
/// `@@last_plan_from_cache` are the v8.5.8 server's. The point get under a sort or a top-n
/// is cached and reused with other ids (an id with no row reads none); `ORDER BY RAND()` is
/// never cached, and its column follows each EXECUTE's parameter. `LIMIT ?` keys the cached
/// plan by the count, and `LIMIT 0` plans as `TableDual` (not cached, with Go's warning).
#[test]
fn prepared_sort_and_top_n_over_dual_and_point_get() {
    use tidb_base::mysql::{BinaryFlag, NotNullFlag, TypeLonglong, TypeVarString};
    let _serial = PREPARED_COUNT.lock();
    let rt = multi_thread_runtime();
    let _rt = rt.enter();
    let (_store, mut s) = sysbench_cached_session();
    let int = new_int_datum;
    let param = |tp, decimal, flag, charset| expr_col("?", tp, -1, decimal, flag, charset);
    let at_prepare = param(TypeVarString, -1, 0, "utf8mb4");
    let int_param = param(TypeLonglong, 0, (NotNullFlag | BinaryFlag) as u32, "binary");
    let str_param = param(TypeVarString, -1, NotNullFlag as u32, "utf8mb4");
    type Exec = (Vec<Datum>, Vec<Col>, Vec<Vec<String>>, bool);
    let cases: Vec<(&str, Vec<Col>, Vec<Exec>)> = vec![
        (
            "SELECT c FROM sbtest1 WHERE id = ? ORDER BY c",
            vec![col_c()],
            vec![
                (vec![int(1)], vec![col_c()], texts(&[&["c1"]]), false),
                (vec![int(42)], vec![col_c()], texts(&[&["c42"]]), true),
                (vec![int(101)], vec![col_c()], vec![], true),
            ],
        ),
        (
            "SELECT c FROM sbtest1 WHERE id = ? ORDER BY c LIMIT 1",
            vec![col_c()],
            vec![
                (vec![int(1)], vec![col_c()], texts(&[&["c1"]]), false),
                (vec![int(42)], vec![col_c()], texts(&[&["c42"]]), true),
                (vec![int(101)], vec![col_c()], vec![], true),
            ],
        ),
        (
            "SELECT ?, c FROM sbtest1 WHERE id = ? ORDER BY c LIMIT 1",
            vec![at_prepare.clone(), col_c()],
            vec![
                (
                    vec![int(7), int(1)],
                    vec![int_param.clone(), col_c()],
                    texts(&[&["7", "c1"]]),
                    false,
                ),
                (
                    vec![int(8), int(42)],
                    vec![int_param.clone(), col_c()],
                    texts(&[&["8", "c42"]]),
                    true,
                ),
            ],
        ),
        (
            "SELECT ? FROM DUAL ORDER BY RAND()",
            vec![at_prepare.clone()],
            vec![
                (
                    vec![int(7)],
                    vec![int_param.clone()],
                    texts(&[&["7"]]),
                    false,
                ),
                (vec![int(8)], vec![int_param], texts(&[&["8"]]), false),
            ],
        ),
        // A new statement for a string parameter: Go's columns are `VARCHAR NOT NULL`
        // (charset 46, length 5, type 253, flags 1, decimals 31).
        (
            "SELECT ? FROM DUAL ORDER BY RAND()",
            vec![at_prepare],
            vec![
                (
                    vec![new_string_datum("x")],
                    vec![str_param.clone()],
                    texts(&[&["x"]]),
                    false,
                ),
                (
                    vec![new_string_datum("y")],
                    vec![str_param],
                    texts(&[&["y"]]),
                    false,
                ),
            ],
        ),
    ];
    for (sql, prepared, execs) in cases {
        let info = block_on(s.prepare_stmt(sql)).unwrap();
        let got: Vec<Col> = info.fields.iter().map(col_of).collect();
        assert_eq!(got, prepared, "PREPARE {sql}");
        for (params, cols, rows, hit) in execs {
            let what = format!("EXECUTE {sql} {params:?}");
            let (got_cols, got_rows) = run_prepared(&mut s, info.stmt_id, params)
                .unwrap_or_else(|e| panic!("{what}: {e}"));
            assert_eq!(
                s.vars().found_in_plan_cache,
                hit,
                "{what}: FoundInPlanCache"
            );
            assert_eq!(got_cols, cols, "{what}");
            assert_eq!(got_rows, rows, "{what}");
        }
        s.drop_prepared_stmt(info.stmt_id).unwrap();
        s.clean_closed_prepared_stmts();
    }
    // `LIMIT ?` keys the cached plan by the count: ids 1, 42, 3, 4 with counts 1, 2, 0, 1
    // miss, miss, miss (TableDual, not cached, with warning), hit.
    let sql = "SELECT c FROM sbtest1 WHERE id = ? ORDER BY c LIMIT ?";
    let info = block_on(s.prepare_stmt(sql)).unwrap();
    let prepared: Vec<Col> = info.fields.iter().map(col_of).collect();
    assert_eq!(prepared, vec![col_c()], "PREPARE {sql}");
    type LimitExec = (Vec<Datum>, Vec<Vec<String>>, bool, Vec<&'static str>);
    let limit_execs: Vec<LimitExec> = vec![
        (vec![int(1), int(1)], texts(&[&["c1"]]), false, vec![]),
        (vec![int(42), int(2)], texts(&[&["c42"]]), false, vec![]),
        (
            vec![int(3), int(0)],
            vec![],
            false,
            vec!["skip prepared plan-cache: get a TableDual plan"],
        ),
        (vec![int(4), int(1)], texts(&[&["c4"]]), true, vec![]),
    ];
    for (params, rows, hit, expected_warns) in limit_execs {
        let what = format!("EXECUTE {sql} {params:?}");
        let (got_cols, got_rows) =
            run_prepared(&mut s, info.stmt_id, params).unwrap_or_else(|e| panic!("{what}: {e}"));
        assert_eq!(
            s.vars().found_in_plan_cache,
            hit,
            "{what}: FoundInPlanCache"
        );
        assert_eq!(got_cols, vec![col_c()], "{what}");
        assert_eq!(got_rows, rows, "{what}");
        let warns: Vec<String> = s
            .vars()
            .stmt_ctx()
            .get_warnings()
            .iter()
            .map(|w| String::from_utf8_lossy(&w.err.error()).into_owned())
            .collect();
        assert_eq!(warns, expected_warns, "{what}: warnings");
    }
    s.drop_prepared_stmt(info.stmt_id).unwrap();
    s.clean_closed_prepared_stmts();
}

/// Regression for #370 (`tools/ab/workloads/prepared.sql:92`): `SELECT c FROM sbtest1 WHERE
/// id = ? LIMIT ?` with `LIMIT 0` misses the `LIMIT 1` cache entry, builds `TableDual`, emits
/// Go's `Warning 1105 skip prepared plan-cache: get a TableDual plan`, and does not cache the
/// plan (`@@last_plan_from_cache = 0`). Also covers `LIMIT ?, ?` (offset + count) and a
/// negative limit argument (warning + error 1210), and checks that a later `PREPARE ... LIMIT ?`
/// in the same session does not read stale parameters left by an earlier `EXECUTE`.
#[test]
fn prepared_limit_param_zero_warns_and_skips_plan_cache() {
    use tidb_base::contextutil::WarnLevelWarning;
    let _serial = PREPARED_COUNT.lock();
    let (_store, mut s) = sysbench_cached_session();
    let int = new_int_datum;
    let warnings_of = |s: &Session| -> Vec<(String, String)> {
        s.vars()
            .stmt_ctx()
            .get_warnings()
            .iter()
            .map(|w| {
                (
                    w.level.as_ref().to_owned(),
                    String::from_utf8_lossy(&w.err.error()).into_owned(),
                )
            })
            .collect()
    };

    // Exact sequence of prepared.sql:90-94 plus cache-hit and repeat-zero checks.
    let sql = "SELECT c FROM sbtest1 WHERE id = ? LIMIT ?";
    let info = block_on(s.prepare_stmt(sql)).unwrap();
    assert_eq!(
        info.fields.iter().map(col_of).collect::<Vec<_>>(),
        vec![col_c()],
        "PREPARE {sql}"
    );
    assert!(warnings_of(&s).is_empty(), "PREPARE warnings");

    type WarnExec = (
        Vec<Datum>,
        Vec<Vec<String>>,
        bool,
        Vec<(&'static str, &'static str)>,
    );
    let execs: Vec<WarnExec> = vec![
        (vec![int(5), int(1)], texts(&[&["c5"]]), false, vec![]),
        (
            vec![int(6), int(0)],
            vec![],
            false,
            vec![(
                WarnLevelWarning,
                "skip prepared plan-cache: get a TableDual plan",
            )],
        ),
        (vec![int(7), int(10)], texts(&[&["c7"]]), false, vec![]),
        // Same LIMIT 1 key as EXECUTE 1: cache hit, no warning.
        (vec![int(8), int(1)], texts(&[&["c8"]]), true, vec![]),
        // Repeat LIMIT 0: TableDual was not cached, so miss + same warning again.
        (
            vec![int(9), int(0)],
            vec![],
            false,
            vec![(
                WarnLevelWarning,
                "skip prepared plan-cache: get a TableDual plan",
            )],
        ),
    ];
    for (params, rows, hit, expected_warns) in execs {
        let what = format!("EXECUTE {sql} {params:?}");
        let (got_cols, got_rows) =
            run_prepared(&mut s, info.stmt_id, params).unwrap_or_else(|e| panic!("{what}: {e}"));
        assert_eq!(got_cols, vec![col_c()], "{what}");
        assert_eq!(got_rows, rows, "{what}");
        assert_eq!(
            s.vars().found_in_plan_cache,
            hit,
            "{what}: last_plan_from_cache"
        );
        let expected: Vec<(String, String)> = expected_warns
            .into_iter()
            .map(|(lvl, m)| (lvl.to_owned(), m.to_owned()))
            .collect();
        assert_eq!(warnings_of(&s), expected, "{what}: warnings");
    }

    // Negative LIMIT argument: NewPlanCacheKey skips cache with warning, build_limit rejects
    // with 1210 (ErrWrongArguments: Incorrect arguments to LIMIT).
    let err = run_prepared(&mut s, info.stmt_id, vec![int(5), int(-1)]).unwrap_err();
    assert_eq!(err.code(), 1210, "negative LIMIT: {err}");
    assert_eq!(
        warnings_of(&s),
        vec![(
            WarnLevelWarning.to_owned(),
            "skip prepared plan-cache: unexpected value after LIMIT".to_owned()
        )]
    );

    // A fresh PREPARE after an EXECUTE with an invalid (-1) parameter still succeeds because
    // get_uint_from_node checks in_execute (false during PREPARE).
    let sql2 = "SELECT c FROM sbtest1 WHERE id = ? LIMIT ?, ?";
    let info2 = block_on(s.prepare_stmt(sql2)).unwrap();
    assert_eq!(
        info2.fields.iter().map(col_of).collect::<Vec<_>>(),
        vec![col_c()],
        "PREPARE {sql2}"
    );
    // offset = 0, count = 0 -> TableDual (warning, not cached).
    let (cols, rows) = run_prepared(&mut s, info2.stmt_id, vec![int(5), int(0), int(0)]).unwrap();
    assert_eq!(cols, vec![col_c()]);
    assert!(rows.is_empty());
    assert!(!s.vars().found_in_plan_cache);
    assert_eq!(
        warnings_of(&s),
        vec![(
            WarnLevelWarning.to_owned(),
            "skip prepared plan-cache: get a TableDual plan".to_owned()
        )]
    );
    // offset = 1, count = 1 -> PhysicalLimit over PointGet (0 rows, cached on second EXECUTE).
    let (cols, rows) = run_prepared(&mut s, info2.stmt_id, vec![int(5), int(1), int(1)]).unwrap();
    assert_eq!(cols, vec![col_c()]);
    assert!(rows.is_empty());
    assert!(!s.vars().found_in_plan_cache);
    assert!(warnings_of(&s).is_empty());
    let (cols, rows) = run_prepared(&mut s, info2.stmt_id, vec![int(6), int(1), int(1)]).unwrap();
    assert_eq!(cols, vec![col_c()]);
    assert!(rows.is_empty());
    assert!(s.vars().found_in_plan_cache);
    assert!(warnings_of(&s).is_empty());

    s.drop_prepared_stmt(info2.stmt_id).unwrap();
    s.drop_prepared_stmt(info.stmt_id).unwrap();
    s.clean_closed_prepared_stmts();
}

/// An unsigned 64-bit parameter selected as a column (the binary protocol's
/// `MYSQL_TYPE_LONGLONG` with the unsigned flag, decoded to a `uint64` datum): each EXECUTE's
/// column follows its own parameter, `BIGINT UNSIGNED NOT NULL BINARY` (wire flags 0x00a1)
/// for `18446744073709551615` and `BIGINT NOT NULL BINARY` (0x0081) for a signed value.
///
/// A v8.5.8 server sends the same for the first EXECUTE of a statement, but its connection
/// keeps the column definitions of that first EXECUTE in the prepared statement
/// (`tidbResultSet.Columns`, `pkg/server/internal/resultset/resultset.go:108-131`, until a
/// schema change, `pkg/planner/core/plan_cache.go:151`) and reuses them when a later EXECUTE
/// binds a parameter of another type: after a signed first EXECUTE it describes
/// `18446744073709551615` as signed, and a client reads -1. tidb-rs keeps no such cache (its
/// server's `ResultSet::columns`, an L2 difference), so the flags below are the current
/// behaviour, kept until the owner rules on it (issue #367; the basic executors spec, D12.2,
/// §4 known differences).
#[test]
fn prepared_unsigned_parameter_selected_as_a_column() {
    use tidb_base::mysql::{BinaryFlag, NotNullFlag, TypeLonglong, UnsignedFlag};
    use tidb_types::datum::new_uint_datum;
    let _serial = PREPARED_COUNT.lock();
    let rt = multi_thread_runtime();
    let _rt = rt.enter();
    let (_store, mut s) = sysbench_cached_session();
    let signed = expr_col(
        "?",
        TypeLonglong,
        -1,
        0,
        (NotNullFlag | BinaryFlag) as u32,
        "binary",
    );
    let unsigned = expr_col(
        "?",
        TypeLonglong,
        -1,
        0,
        (NotNullFlag | BinaryFlag | UnsignedFlag) as u32,
        "binary",
    );
    let max = || new_uint_datum(u64::MAX);
    let five = || new_int_datum(5);
    let id = || new_int_datum(1);
    let with_c = |p: Col| vec![p, col_c()];
    type Exec = (Vec<Datum>, Vec<Col>, Vec<Vec<String>>);
    let cases: Vec<(&str, Vec<Exec>)> = vec![
        // Unsigned first, then signed (Go: both described as unsigned).
        (
            "SELECT ?, c FROM sbtest1 WHERE id=?",
            vec![
                (
                    vec![max(), id()],
                    with_c(unsigned.clone()),
                    texts(&[&["18446744073709551615", "c1"]]),
                ),
                (
                    vec![five(), id()],
                    with_c(signed.clone()),
                    texts(&[&["5", "c1"]]),
                ),
            ],
        ),
        // Signed first, then unsigned (Go: both described as signed, the second read as -1).
        (
            "SELECT ?, c FROM sbtest1 WHERE id=?",
            vec![
                (
                    vec![five(), id()],
                    with_c(signed.clone()),
                    texts(&[&["5", "c1"]]),
                ),
                (
                    vec![max(), id()],
                    with_c(unsigned.clone()),
                    texts(&[&["18446744073709551615", "c1"]]),
                ),
            ],
        ),
        (
            "SELECT ?, c FROM sbtest1 WHERE id=? ORDER BY c LIMIT 1",
            vec![
                (
                    vec![five(), id()],
                    with_c(signed.clone()),
                    texts(&[&["5", "c1"]]),
                ),
                (
                    vec![max(), id()],
                    with_c(unsigned.clone()),
                    texts(&[&["18446744073709551615", "c1"]]),
                ),
            ],
        ),
        (
            "SELECT ? FROM DUAL ORDER BY RAND()",
            vec![
                (vec![five()], vec![signed], texts(&[&["5"]])),
                (
                    vec![max()],
                    vec![unsigned],
                    texts(&[&["18446744073709551615"]]),
                ),
            ],
        ),
    ];
    for (sql, execs) in cases {
        let info = block_on(s.prepare_stmt(sql)).unwrap();
        for (params, cols, rows) in execs {
            let what = format!("EXECUTE {sql} {params:?}");
            let (got_cols, got_rows) = run_prepared(&mut s, info.stmt_id, params)
                .unwrap_or_else(|e| panic!("{what}: {e}"));
            assert_eq!(got_cols, cols, "{what}");
            assert_eq!(got_rows, rows, "{what}");
        }
        s.drop_prepared_stmt(info.stmt_id).unwrap();
        s.clean_closed_prepared_stmts();
    }
}
