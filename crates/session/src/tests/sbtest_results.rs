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

/// Puts the row `(id, 10 × id, "c<id>", "p<id>")` and its secondary index `k_1` (index id 1)
/// entry `(k, id)` into the mock store.
fn put_sbtest1_row(store: &MockStore, table_id: i64, id: i64) {
    let mut enc = tidb_tablecodec::rowcodec::Encoder::default();
    enc.enable = true;
    let mut c = Datum::default();
    c.set_string(format!("c{id}").into_bytes(), "utf8mb4_bin".to_owned());
    let mut pad = Datum::default();
    pad.set_string(format!("p{id}").into_bytes(), "utf8mb4_bin".to_owned());
    let k_val = id.wrapping_mul(10);
    let row = [new_int_datum(k_val), c, pad];
    let val = tidb_tablecodec::encode_row(None, &row, &[2, 3, 4], None, Some(&mut enc)).unwrap();
    let key = tidb_tablecodec::encode_row_key_with_handle(
        table_id,
        &tidb_kv::Handle::Int(tidb_kv::key::IntHandle(id)),
    );
    let mut idx_val = Vec::new();
    tidb_codec::codec::encode_key(
        &tidb_base::gostd::time::utc(),
        &mut idx_val,
        &[new_int_datum(k_val), new_int_datum(id)],
    )
    .unwrap();
    let idx_key = tidb_tablecodec::encode_index_seek_key(table_id, 1, &idx_val);
    let mut rows = store.rows.lock();
    rows.insert(key.0, val);
    rows.insert(idx_key.0, vec![b'0']);
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

/// Ranges over the primary key now run end-to-end on the new stack (`TableReaderExecutor` +
/// `distsql` + `SortExec` / `TopNExec` / `LimitExec` / `SelectionExec` / `ProjectionExec`),
/// as text queries and as prepared statements (both plan cache miss and plan cache hit with
/// rebuilt ranges), verifying exact rows AND exact column metadata (names, types, flen,
/// decimal, flags, charset) matching Go v8.5.8.
#[test]
fn ranges_run_on_new_stack_with_exact_rows_and_column_metadata() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let _enter = rt.enter();
    let _serial = PREPARED_COUNT.lock();
    let (_store, mut s) = sysbench_cached_session();
    let expected_c_rows = |lo: i64, hi: i64| -> Vec<Vec<String>> {
        let start = lo.max(1);
        let end = hi.min(100);
        if start > end {
            return Vec::new();
        }
        (start..=end).map(|id| vec![format!("c{id}")]).collect()
    };
    let ranges = [(1, 10), (-100, 50), (i64::MIN, i64::MAX), (5, 104), (1, 10)];
    for (lo, hi) in ranges {
        let sql = format!("SELECT c FROM sbtest1 WHERE id BETWEEN {lo} AND {hi}");
        let (cols, rows) = run_text(&mut s, &sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
        assert_eq!(cols, vec![col_c()], "{sql}");
        assert_eq!(rows, expected_c_rows(lo, hi), "{sql}");
    }
    let sql = "SELECT c FROM sbtest1 WHERE id BETWEEN ? AND ?";
    let info = block_on(s.prepare_stmt(sql)).unwrap();
    let prepared_cols: Vec<Col> = info.fields.iter().map(col_of).collect();
    assert_eq!(prepared_cols, vec![col_c()]);
    for (idx, (lo, hi)) in ranges.into_iter().enumerate() {
        let what = format!("{sql} ({lo}, {hi})");
        let (cols, rows) = run_prepared(
            &mut s,
            info.stmt_id,
            vec![new_int_datum(lo), new_int_datum(hi)],
        )
        .unwrap_or_else(|e| panic!("{what}: {e}"));
        assert_eq!(
            s.vars().found_in_plan_cache,
            idx > 0,
            "{what}: FoundInPlanCache"
        );
        assert_eq!(cols, prepared_cols, "{what}");
        assert_eq!(rows, expected_c_rows(lo, hi), "{what}");
    }
    s.drop_prepared_stmt(info.stmt_id).unwrap();

    // ORDER BY c (Sort > TableReader > TableRangeScan)
    let order_sql = "SELECT c FROM sbtest1 WHERE id BETWEEN 1 AND 12 ORDER BY c";
    let (cols, rows) = run_text(&mut s, order_sql).unwrap();
    assert_eq!(cols, vec![col_c()]);
    let mut want_ordered: Vec<Vec<String>> = (1..=12).map(|id| vec![format!("c{id}")]).collect();
    want_ordered.sort();
    assert_eq!(rows, want_ordered);

    // ORDER BY id DESC LIMIT 5 (Limit > TableReader > Limit > TableFullScan desc)
    let desc_sql = "SELECT id, c FROM sbtest1 ORDER BY id DESC LIMIT 5";
    let (cols, rows) = run_text(&mut s, desc_sql).unwrap();
    assert_eq!(cols, vec![col_id(), col_c()]);
    let want_desc: Vec<Vec<String>> = (96..=100)
        .rev()
        .map(|id| vec![id.to_string(), format!("c{id}")])
        .collect();
    assert_eq!(rows, want_desc);

    // Full table scan
    let full_sql = "SELECT id, c FROM sbtest1";
    let (cols, rows) = run_text(&mut s, full_sql).unwrap();
    assert_eq!(cols, vec![col_id(), col_c()]);
    assert_eq!(rows.len(), 100);
    assert_eq!(rows[0], vec!["1".to_owned(), "c1".to_owned()]);
    assert_eq!(rows[99], vec!["100".to_owned(), "c100".to_owned()]);
}

/// Single-read covering `IndexReader` queries over `sbtest1` (`KEY k_1 (k)`) and multi-point
/// `OR` grouped-range `IndexReader` queries with `ORDER BY` (`needMergeSort`) execute
/// end-to-end on the Rust stack against the in-memory coprocessor store, checking BOTH exact
/// rows AND exact column metadata across text protocol and `PREPARE`/`EXECUTE` (plan cache
/// miss and hit with varying bounds).
#[test]
fn index_reader_runs_on_new_stack_with_exact_rows_and_column_metadata() {
    use tidb_base::mysql::{MultipleKeyFlag, NotNullFlag, PriKeyFlag, TypeLong};
    use tidb_model::{ColumnInfo, DBInfo, IndexColumn, IndexInfo, StatePublic, TableInfo};
    use tidb_parser::model::CIStr;
    use tidb_parser::types::{FieldType, UnspecifiedLength};

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let _enter = rt.enter();
    let _serial = PREPARED_COUNT.lock();
    let (_store, mut s) = sysbench_cached_session();

    let col_k = Col {
        name: "k".to_owned(),
        org_name: "k".to_owned(),
        table: "sbtest1".to_owned(),
        org_table: "sbtest1".to_owned(),
        schema: "test".to_owned(),
        tp: TypeLong,
        flen: 11,
        decimal: 0,
        flag: (NotNullFlag | MultipleKeyFlag) as u32,
        charset: "binary".to_owned(),
    };

    // 1. SELECT k FROM sbtest1 WHERE k BETWEEN ... (text + prepared with plan cache hit/miss).
    let expected_k = |lo: i64, hi: i64| -> Vec<Vec<String>> {
        (1..=100_i64)
            .map(|id| id * 10)
            .filter(|&k| k >= lo && k <= hi)
            .map(|k| vec![k.to_string()])
            .collect()
    };
    let bounds = [(10, 50), (25, 85), (1000, 1000), (2000, 3000), (10, 50)];
    for (lo, hi) in bounds {
        let sql = format!("SELECT k FROM sbtest1 WHERE k BETWEEN {lo} AND {hi}");
        let (cols, rows) = run_text(&mut s, &sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
        assert_eq!(cols, vec![col_k.clone()], "{sql}");
        assert_eq!(rows, expected_k(lo, hi), "{sql}");
    }
    let sql_k = "SELECT k FROM sbtest1 WHERE k BETWEEN ? AND ?";
    let info_k = block_on(s.prepare_stmt(sql_k)).unwrap();
    let prepared_k: Vec<Col> = info_k.fields.iter().map(col_of).collect();
    assert_eq!(prepared_k, vec![col_k.clone()]);
    for (idx, (lo, hi)) in bounds.into_iter().enumerate() {
        let what = format!("{sql_k} ({lo}, {hi})");
        let (cols, rows) = run_prepared(
            &mut s,
            info_k.stmt_id,
            vec![new_int_datum(lo), new_int_datum(hi)],
        )
        .unwrap_or_else(|e| panic!("{what}: {e}"));
        // Equal bounds (1000, 1000) collapse a range predicate into a point range, which causes
        // TiDB's range builder to skip plan-cache reuse for safety; non-point bounds hit cache.
        assert_eq!(
            s.vars().found_in_plan_cache,
            idx > 0 && lo != hi,
            "{what}: FoundInPlanCache"
        );
        assert_eq!(cols, prepared_k, "{what}");
        assert_eq!(rows, expected_k(lo, hi), "{what}");
    }
    s.drop_prepared_stmt(info_k.stmt_id).unwrap();

    // 2. SELECT id, k FROM sbtest1 WHERE k BETWEEN ... (exercises OutputOffsets [1, 0] since
    // index scan schema is [k, id] while output schema is [id, k]).
    let expected_id_k = |lo: i64, hi: i64| -> Vec<Vec<String>> {
        (1..=100_i64)
            .filter(|&id| {
                let k = id * 10;
                k >= lo && k <= hi
            })
            .map(|id| vec![id.to_string(), (id * 10).to_string()])
            .collect()
    };
    let sql_idk = "SELECT id, k FROM sbtest1 WHERE k BETWEEN ? AND ?";
    let info_idk = block_on(s.prepare_stmt(sql_idk)).unwrap();
    let prepared_idk: Vec<Col> = info_idk.fields.iter().map(col_of).collect();
    assert_eq!(prepared_idk, vec![col_id(), col_k.clone()]);
    for (idx, (lo, hi)) in [(10, 30), (950, 1000), (15, 25)].into_iter().enumerate() {
        let what = format!("{sql_idk} ({lo}, {hi})");
        let (cols, rows) = run_prepared(
            &mut s,
            info_idk.stmt_id,
            vec![new_int_datum(lo), new_int_datum(hi)],
        )
        .unwrap_or_else(|e| panic!("{what}: {e}"));
        // Equal bounds (1000, 1000) collapse a range predicate into a point range, which causes
        // TiDB's range builder to skip plan-cache reuse for safety; non-point bounds hit cache.
        assert_eq!(
            s.vars().found_in_plan_cache,
            idx > 0 && lo != hi,
            "{what}: FoundInPlanCache"
        );
        assert_eq!(cols, prepared_idk, "{what}");
        assert_eq!(rows, expected_id_k(lo, hi), "{what}");
    }
    s.drop_prepared_stmt(info_idk.stmt_id).unwrap();

    // 3. SELECT k FROM sbtest1 ORDER BY k LIMIT 3 (and DESC LIMIT 3).
    let (cols, rows) = run_text(&mut s, "SELECT k FROM sbtest1 ORDER BY k LIMIT 3").unwrap();
    assert_eq!(cols, vec![col_k.clone()]);
    assert_eq!(rows, texts(&[&["10"], &["20"], &["30"]]));

    let (cols, rows) = run_text(&mut s, "SELECT k FROM sbtest1 ORDER BY k DESC LIMIT 3").unwrap();
    assert_eq!(cols, vec![col_k]);
    assert_eq!(rows, texts(&[&["1000"], &["990"], &["980"]]));

    // 4. Multi-point OR grouped-range IndexReader with ORDER BY (exercises needMergeSort +
    // NewSortedSelectResults end-to-end on composite secondary index idx_ab (a, b)).
    let mk_int = |id: i64, name: &str, offset: isize, flags: usize| {
        let mut ft = FieldType::new(TypeLong);
        ft.set_flen(11);
        ft.set_decimal(0);
        ft.set_charset("binary");
        ft.set_collate("binary");
        ft.add_flag(flags);
        Arc::new(ColumnInfo {
            id,
            name: CIStr::new(name),
            offset,
            field_type: ft,
            state: StatePublic,
            ..Default::default()
        })
    };
    let t_idx = TableInfo {
        id: 210,
        dbid: TEST_DB_ID,
        name: CIStr::new("t_idx"),
        columns: Some(vec![
            mk_int(1, "id", 0, NotNullFlag | PriKeyFlag),
            mk_int(2, "a", 1, NotNullFlag | MultipleKeyFlag),
            mk_int(3, "b", 2, NotNullFlag),
        ]),
        indices: Some(vec![Arc::new(IndexInfo {
            id: 1,
            name: CIStr::new("idx_ab"),
            table: CIStr::new("t_idx"),
            columns: Some(vec![
                IndexColumn {
                    name: CIStr::new("a"),
                    offset: 1,
                    length: UnspecifiedLength,
                    ..Default::default()
                },
                IndexColumn {
                    name: CIStr::new("b"),
                    offset: 2,
                    length: UnspecifiedLength,
                    ..Default::default()
                },
            ]),
            state: StatePublic,
            ..Default::default()
        })]),
        pk_is_handle: true,
        state: StatePublic,
        version: 5,
        max_column_id: 3,
        max_index_id: 1,
        ..Default::default()
    };
    let mut db = DBInfo {
        id: TEST_DB_ID,
        name: CIStr::new("test"),
        state: StatePublic,
        ..Default::default()
    };
    db.deprecated.tables = Some(vec![Arc::new(t_idx)]);
    let mut isb = tidb_infoschema::new_builder(None, None, tidb_infoschema::new_data());
    block_on(isb.init_with_db_infos(vec![db], vec![], vec![], 1)).unwrap();
    let is_idx = Arc::new(block_on(isb.build(1)));

    let store_idx = MockStore::new(10);
    // Insert interleaved b values across a = 1 and a = 3 so merge sort across groups is required:
    // a=1: b in {10, 40, 70}; a=3: b in {20, 30, 50}.
    for (id, a, b) in [
        (1_i64, 1_i64, 10_i64),
        (2, 1, 40),
        (3, 1, 70),
        (4, 3, 20),
        (5, 3, 30),
        (6, 3, 50),
    ] {
        let mut encoded = Vec::new();
        tidb_codec::codec::encode_key(
            &tidb_base::gostd::time::utc(),
            &mut encoded,
            &[new_int_datum(a), new_int_datum(b), new_int_datum(id)],
        )
        .unwrap();
        let idx_key = tidb_tablecodec::encode_index_seek_key(210, 1, &encoded);
        store_idx.rows.lock().insert(idx_key.0, vec![b'0']);
    }
    let cache_idx = new_test_cache();
    let mut s_idx = cached_session(&store_idx, is_idx, &cache_idx);
    // Disable index merge (D10.7) via the real system variable so the OR predicate stays on a
    // single composite index scan and exercises grouped-range IndexReader + needMergeSort.
    s_idx
        .vars_mut()
        .set_system_var("tidb_enable_index_merge", "OFF")
        .unwrap();

    // 4a. Multi-point OR grouped-range IndexReader with ORDER BY ASC: verify the compiled plan
    // is Limit > IndexReader > Limit > IndexScan with 2 grouped ranges and non-empty by_items.
    let merge_asc_sql = "SELECT a, b FROM t_idx WHERE (a = 1 OR a = 3) ORDER BY b LIMIT 4";
    {
        use tidb_planner::base::PhysicalPlan;
        use tidb_planner::core::physical_plans::{PhysicalIndexReader, PhysicalIndexScan};
        block_on(s_idx.finish_unfinished_stmt());
        block_on(s_idx.core.prepare_txn_ctx()).unwrap();
        let node = parse(merge_asc_sql);
        reset_context_of_stmt(&mut s_idx.core, &node, merge_asc_sql).unwrap();
        s_idx
            .core
            .txn_manager
            .on_stmt_start(&mut s_idx.core.sctx, Some(&node))
            .unwrap();
        let stmt = tidb_executor::compiler::Compiler
            .compile(&mut s_idx.core, node, merge_asc_sql)
            .unwrap();
        let plan = stmt.plan.as_ref().expect("compiled plan");
        fn find_index_reader(p: &dyn PhysicalPlan) -> Option<&PhysicalIndexReader> {
            if let Some(ir) = p.as_any().downcast_ref::<PhysicalIndexReader>() {
                return Some(ir);
            }
            p.children().iter().find_map(|c| find_index_reader(&**c))
        }
        let ir = find_index_reader(&**plan).expect("expected IndexReader in plan");
        let is = ir.index_plans[0]
            .as_any()
            .downcast_ref::<PhysicalIndexScan>()
            .expect("expected IndexScan inside IndexReader");
        assert_eq!(is.grouped_ranges.len(), 2);
        assert!(!is.by_items.is_empty());
        s_idx.core.rollback_on_error();
        s_idx.core.txn_manager.on_stmt_end();
    }
    let (cols, rows) = run_text(&mut s_idx, merge_asc_sql).unwrap();
    assert_eq!(cols.len(), 2);
    assert_eq!(cols[0].name, "a");
    assert_eq!(cols[1].name, "b");
    assert_eq!(
        rows,
        texts(&[&["1", "10"], &["3", "20"], &["3", "30"], &["1", "40"]])
    );

    // 4b. Multi-point OR grouped-range IndexReader with ORDER BY DESC.
    let merge_desc_sql = "SELECT a, b FROM t_idx WHERE (a = 1 OR a = 3) ORDER BY b DESC LIMIT 3";
    let (cols, rows) = run_text(&mut s_idx, merge_desc_sql).unwrap();
    assert_eq!(cols.len(), 2);
    assert_eq!(rows, texts(&[&["1", "70"], &["3", "50"], &["1", "40"]]));
}

/// `SUM(k)` over a range (sysbench's sum range) builds the logical aggregation (D09.4 part A)
/// and falls back at logical optimization (`BuildKeySolver`, D09.13), before any executor
/// opened, as a text query and as a prepared statement, also for a range of one point, which
/// Go plans as a stream aggregation over a projection over a point get.
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
        assert_eq!(assert_falls_back(&s, sql, before, r), "D09.13");
    }
    let sql = "SELECT SUM(k) FROM sbtest1 WHERE id BETWEEN ? AND ?";
    let before = s.executed_stmts();
    // PREPARE of an aggregate statement completes on the Rust stack (plan_cache_utils.rs);
    // pin the exact column metadata against Go TiDB v8.5.8 (SUM(INT) -> DECIMAL(32, 0),
    // nullable with BinaryFlag, name "SUM(k)").
    let info = block_on(s.prepare_stmt(sql)).expect("PREPARE of aggregate succeeds on Rust stack");
    assert_eq!(s.executed_stmts(), before);
    let prepared: Vec<Col> = info.fields.iter().map(col_of).collect();
    assert_eq!(
        prepared,
        vec![Col {
            name: "SUM(k)".to_owned(),
            org_name: String::new(),
            table: String::new(),
            org_table: String::new(),
            schema: String::new(),
            tp: tidb_base::mysql::TypeNewDecimal,
            flen: 32,
            decimal: 0,
            flag: tidb_base::mysql::BinaryFlag as u32,
            charset: "binary".to_owned(),
        }],
        "PREPARE column metadata for {sql}"
    );
    for (lo, hi) in [(1, 10), (5, 5)] {
        let what = format!("{sql} ({lo}, {hi})");
        let r = run_prepared(
            &mut s,
            info.stmt_id,
            vec![new_int_datum(lo), new_int_datum(hi)],
        );
        assert_eq!(assert_falls_back(&s, &what, before, r), "D09.13");
    }
    s.drop_prepared_stmt(info.stmt_id).unwrap();
    s.clean_closed_prepared_stmts();

    // Also pin PREPARE metadata for COUNT(*) + MAX(k) against Go TiDB v8.5.8.
    let sql2 = "SELECT COUNT(*), MAX(k) FROM sbtest1 WHERE id BETWEEN ? AND ?";
    let info2 = block_on(s.prepare_stmt(sql2)).expect("PREPARE COUNT(*), MAX(k)");
    let prepared2: Vec<Col> = info2.fields.iter().map(col_of).collect();
    assert_eq!(
        prepared2,
        vec![
            Col {
                name: "COUNT(*)".to_owned(),
                org_name: String::new(),
                table: String::new(),
                org_table: String::new(),
                schema: String::new(),
                tp: tidb_base::mysql::TypeLonglong,
                flen: 21,
                decimal: 0,
                flag: (tidb_base::mysql::NotNullFlag | tidb_base::mysql::BinaryFlag) as u32,
                charset: "binary".to_owned(),
            },
            Col {
                name: "MAX(k)".to_owned(),
                org_name: String::new(),
                table: String::new(),
                org_table: String::new(),
                schema: String::new(),
                tp: tidb_base::mysql::TypeLong,
                flen: 11,
                decimal: 0,
                flag: tidb_base::mysql::MultipleKeyFlag as u32,
                charset: "binary".to_owned(),
            },
        ],
        "PREPARE column metadata for {sql2}"
    );
    s.drop_prepared_stmt(info2.stmt_id).unwrap();
    s.clean_closed_prepared_stmts();
}

/// R27 (#437): Drive real session PREPARE + two EXECUTEs (with plan cache disabled) for
/// statements whose SELECT alias is rewritten inside HAVING/GROUP BY. On Go v8.5.8,
/// `SELECT k+1 AS x FROM sbtest1 GROUP BY k+1 HAVING SUM(x) > 0` fails with ERROR 1055
/// starting on the second EXECUTE due to in-place AST mutation. In tidb-rs, both EXECUTEs
/// pass D09.4 aggregation building cleanly and stop at D09.13 (BuildKeySolver).
#[test]
fn prepared_repeated_execute_having_and_group_by_alias_through_session() {
    let _serial = PREPARED_COUNT.lock();
    let (_store, mut s) = sysbench_session(); // plan cache OFF
    for sql in [
        "SELECT k+1 AS x FROM sbtest1 GROUP BY k+1 HAVING SUM(x) > 0",
        "SELECT k+1 AS x FROM sbtest1 GROUP BY x HAVING SUM(x) > 0",
    ] {
        let info = block_on(s.prepare_stmt(sql)).unwrap_or_else(|e| panic!("PREPARE {sql}: {e}"));
        let prepared: Vec<Col> = info.fields.iter().map(col_of).collect();
        assert_eq!(prepared.len(), 1);
        assert_eq!(prepared[0].name, "x");
        for run in 1..=2 {
            let before = s.executed_stmts();
            let r = run_prepared(&mut s, info.stmt_id, vec![]);
            assert_eq!(
                assert_falls_back(&s, &format!("{sql} (EXECUTE #{run})"), before, r),
                "D09.13",
                "EXECUTE #{run} must pass D09.4 without ERROR 1055 for {sql}"
            );
        }
        s.drop_prepared_stmt(info.stmt_id).unwrap();
        s.clean_closed_prepared_stmts();
    }
}

/// Pin PREPARE column metadata for window statements against live Go TiDB v8.5.8 (unistore),
/// including the cross-group #402 case (`LEAD` built before `FIRST_VALUE`, where `LEAD` must
/// be nullable without `NotNullFlag`), and drive real session PREPARE + two EXECUTEs (plan
/// cache OFF) for named windows to verify repeated-EXECUTE idempotence.
#[test]
fn window_prepare_metadata_and_repeated_execute_through_session() {
    let _serial = PREPARED_COUNT.lock();
    let (_store, mut s) = sysbench_session(); // plan cache OFF

    // 1. Plain window query: ROW_NUMBER() + SUM(k) OVER (...).
    let sql_plain =
        "SELECT ROW_NUMBER() OVER (ORDER BY id), SUM(k) OVER (PARTITION BY k) FROM sbtest1";
    let before = s.executed_stmts();
    let info_plain = block_on(s.prepare_stmt(sql_plain)).expect("PREPARE plain window query");
    assert_eq!(s.executed_stmts(), before);
    let cols_plain: Vec<Col> = info_plain.fields.iter().map(col_of).collect();
    assert_eq!(
        cols_plain,
        vec![
            Col {
                name: "ROW_NUMBER() OVER (ORDER BY id)".to_owned(),
                org_name: String::new(),
                table: String::new(),
                org_table: String::new(),
                schema: String::new(),
                tp: tidb_base::mysql::TypeLonglong,
                flen: 21,
                decimal: 0,
                flag: tidb_base::mysql::BinaryFlag as u32,
                charset: "binary".to_owned(),
            },
            Col {
                name: "SUM(k) OVER (PARTITION BY k)".to_owned(),
                org_name: String::new(),
                table: String::new(),
                org_table: String::new(),
                schema: String::new(),
                tp: tidb_base::mysql::TypeNewDecimal,
                flen: 32,
                decimal: 0,
                flag: tidb_base::mysql::BinaryFlag as u32,
                charset: "binary".to_owned(),
            },
        ],
        "PREPARE column metadata for {sql_plain}"
    );
    s.drop_prepared_stmt(info_plain.stmt_id).unwrap();
    s.clean_closed_prepared_stmts();

    // 2. Cross-group #402 case: LEAD's group is built before FIRST_VALUE's group; Go strips
    // NotNullFlag in checkWindowFuncArgs, so LEAD(k, 1, 0) is nullable (flag = BinaryFlag,
    // without NotNullFlag) and FIRST_VALUE(k) has MultipleKeyFlag (without NotNullFlag).
    let sql_cross =
        "SELECT LEAD(k, 1, 0) OVER (ORDER BY k), FIRST_VALUE(k) OVER (ORDER BY id) FROM sbtest1";
    let info_cross = block_on(s.prepare_stmt(sql_cross)).expect("PREPARE cross-group window query");
    let cols_cross: Vec<Col> = info_cross.fields.iter().map(col_of).collect();
    assert_eq!(
        cols_cross,
        vec![
            Col {
                name: "LEAD(k, 1, 0) OVER (ORDER BY k)".to_owned(),
                org_name: String::new(),
                table: String::new(),
                org_table: String::new(),
                schema: String::new(),
                tp: tidb_base::mysql::TypeLonglong,
                flen: 11,
                decimal: 0,
                flag: tidb_base::mysql::BinaryFlag as u32,
                charset: "binary".to_owned(),
            },
            Col {
                name: "FIRST_VALUE(k) OVER (ORDER BY id)".to_owned(),
                org_name: String::new(),
                table: String::new(),
                org_table: String::new(),
                schema: String::new(),
                tp: tidb_base::mysql::TypeLong,
                flen: 11,
                decimal: 0,
                flag: tidb_base::mysql::MultipleKeyFlag as u32,
                charset: "binary".to_owned(),
            },
        ],
        "PREPARE column metadata for {sql_cross}"
    );
    s.drop_prepared_stmt(info_cross.stmt_id).unwrap();
    s.clean_closed_prepared_stmts();

    // 3. Named WINDOW spec: pin PREPARE metadata and drive two real session EXECUTEs (plan cache
    // OFF) to confirm repeated EXECUTE passes D09.4 cleanly and falls back at D10.3.
    let sql_named = "SELECT SUM(k) OVER (w ORDER BY id) FROM sbtest1 WINDOW w AS (PARTITION BY k)";
    let info_named = block_on(s.prepare_stmt(sql_named)).expect("PREPARE named window query");
    let cols_named: Vec<Col> = info_named.fields.iter().map(col_of).collect();
    assert_eq!(
        cols_named,
        vec![Col {
            name: "SUM(k) OVER (w ORDER BY id)".to_owned(),
            org_name: String::new(),
            table: String::new(),
            org_table: String::new(),
            schema: String::new(),
            tp: tidb_base::mysql::TypeNewDecimal,
            flen: 32,
            decimal: 0,
            flag: tidb_base::mysql::BinaryFlag as u32,
            charset: "binary".to_owned(),
        }],
        "PREPARE column metadata for {sql_named}"
    );
    for run in 1..=2 {
        let before = s.executed_stmts();
        let r = run_prepared(&mut s, info_named.stmt_id, vec![]);
        assert_eq!(
            assert_falls_back(&s, &format!("{sql_named} (EXECUTE #{run})"), before, r),
            "D10.3",
            "EXECUTE #{run} must pass D09.4 cleanly for {sql_named}"
        );
    }
    s.drop_prepared_stmt(info_named.stmt_id).unwrap();
    s.clean_closed_prepared_stmts();
}

/// The stream aggregation executor (the aggregation executors D12.3, part A) and logical
/// aggregation builder (D09.4, part A) are ported, but no aggregate reaches the executor yet:
/// `BuildKeySolver` (D09.13), aggregation optimization rules (D09.9), and physical aggregation
/// plans / enumeration (D10.1 / D10.3) are not ported yet, so every statement with an
/// aggregate, a GROUP BY or a DISTINCT falls back at logical optimization (`BuildKeySolver`,
/// D09.13), before any executor opened; also the shapes Go plans as a stream aggregation (an
/// aggregate over a point get or a range of the clustered index). The stop of each statement
/// is pinned, so the first statement that reaches the next stage fails here and gets updated.
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
        assert_eq!(assert_falls_back(&s, sql, before, r), "D09.13", "{sql}");
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

/// The error a statement returns while it starts or while its rows are read (an expression
/// error in a projection comes from `next`); the result set is closed after it, as the
/// connection does.
fn exec_error(s: &mut Session, started: Result<Option<crate::ExecStmtResult>, Error>) -> Error {
    let mut rs = match started {
        Err(e) => return e,
        Ok(rs) => rs.expect("a result set"),
    };
    let mut chk = rs.new_chunk().unwrap();
    let err = loop {
        match block_on(rs.next(s, &mut chk)) {
            Err(e) => break e,
            Ok(()) if chk.num_rows() == 0 => panic!("the statement returned no error"),
            Ok(()) => {}
        }
    };
    block_on(rs.close(s)).unwrap();
    assert!(s.is_idle(), "{}", s.state_snapshot());
    err
}

/// Ruling R37 (#499, owner decision 2026-11-28): signed BIGINT `0 - (-9223372036854775808)`
/// is `ERROR 1690 (22003) BIGINT value is out of range in '(0 - -9223372036854775808)'`, as in
/// MySQL and TiKV, where Go v8.5.8 wraps it to `-9223372036854775808` (its overflow check
/// tests `a > 0`, not `a >= 0`, `builtinArithmeticMinusIntSig.overflowCheck`). Pinned for an
/// operand computed from a column (the projection's row and vector drivers), for constants
/// (constant folding fails, so the expression stays and fails when it runs, as Go's
/// `foldConstant` keeps an expression whose evaluation fails) and for prepared parameters
/// (with the plan cache on: a cached plan fails the same way). The neighbouring values, one
/// away from the limit, still give Go's result.
#[test]
fn signed_zero_minus_min_bigint_is_out_of_range() {
    let _serial = PREPARED_COUNT.lock();
    let (_store, mut s) = sysbench_cached_session();
    let out_of_range = |what: &str, e: &Error, expr: &str| {
        assert_eq!(e.code(), 1690, "{what}: {e}");
        assert_eq!(
            String::from_utf8_lossy(&e.error()),
            format!("[types:1690]BIGINT value is out of range in '{expr}'"),
            "{what}"
        );
    };
    let max = "9223372036854775807";

    // An operand computed from a column; both drivers of the projection.
    for vectorized in [true, false] {
        s.vars_mut().enable_vectorized_expression = vectorized;
        for (sql, expr) in [
            (
                "SELECT 0 - (-9223372036854775807 - id) FROM sbtest1 WHERE id = 1",
                "(0 - minus(-9223372036854775807, test.sbtest1.id))",
            ),
            (
                "SELECT (id - 1) - (-9223372036854775808) FROM sbtest1 WHERE id = 1",
                "(minus(test.sbtest1.id, 1) - -9223372036854775808)",
            ),
        ] {
            let what = format!("{sql} (vectorized {vectorized})");
            let started = block_on(s.execute_stmt(parse(sql), sql));
            let e = exec_error(&mut s, started);
            out_of_range(&what, &e, expr);
        }
        for sql in [
            "SELECT 0 - (-9223372036854775806 - id) FROM sbtest1 WHERE id = 1",
            "SELECT (id - 1) - (-9223372036854775807) FROM sbtest1 WHERE id = 1",
        ] {
            let (_, rows) = run_text(&mut s, sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
            assert_eq!(rows, texts(&[&[max]]), "{sql} (vectorized {vectorized})");
        }
    }
    s.vars_mut().enable_vectorized_expression = true;

    // Constants.
    for sql in [
        "SELECT 0 - (-9223372036854775808)",
        "SELECT 0 - CAST(-9223372036854775808 AS SIGNED)",
    ] {
        let started = block_on(s.execute_stmt(parse(sql), sql));
        let e = exec_error(&mut s, started);
        out_of_range(sql, &e, "(0 - -9223372036854775808)");
    }
    let sql = "SELECT 0 - (-9223372036854775807)";
    let (_, rows) = run_text(&mut s, sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    assert_eq!(rows, texts(&[&[max]]), "{sql}");

    // Prepared parameters; the plan cache is on, so the later EXECUTEs reuse the plan.
    let int = new_int_datum;
    for (sql, err_params, ok_params) in [
        (
            "SELECT 0 - ? FROM sbtest1 WHERE id = ?",
            vec![int(i64::MIN), int(1)],
            vec![int(-i64::MAX), int(1)],
        ),
        (
            "SELECT ? - ?",
            vec![int(0), int(i64::MIN)],
            vec![int(0), int(-i64::MAX)],
        ),
    ] {
        let info = block_on(s.prepare_stmt(sql)).unwrap();
        for params in [err_params.clone(), ok_params, err_params] {
            let what = format!("EXECUTE {sql} {params:?}");
            let fails = params.iter().any(|d| d.get_int64() == i64::MIN);
            if fails {
                let started = block_on(s.execute_prepared_stmt(info.stmt_id, params));
                let e = exec_error(&mut s, started);
                out_of_range(&what, &e, "(0 - -9223372036854775808)");
            } else {
                let (_, rows) = run_prepared(&mut s, info.stmt_id, params)
                    .unwrap_or_else(|e| panic!("{what}: {e}"));
                assert_eq!(rows, texts(&[&[max]]), "{what}");
            }
        }
        s.drop_prepared_stmt(info.stmt_id).unwrap();
        s.clean_closed_prepared_stmts();
    }
}
