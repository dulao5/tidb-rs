//! End-to-end checks against a real cluster, compared with Go TiDB on the same data.
//!
//! Skipped unless `TIDB_RS_PD` (PD address, e.g. `127.0.0.1:23379`) and `TIDB_RS_TIDB_PORT`
//! (Go TiDB's MySQL port on 127.0.0.1, e.g. `25000`) are set; needs the `mysql` client.
//! Creates and drops database `tidb_rs_kv_it`. For example:
//!
//! ```sh
//! tiup playground v8.5.8 --tag maya-kv --db 1 --pd 1 --kv 1 --tiflash 0 --port-offset 21000
//! TIDB_RS_PD=127.0.0.1:23379 TIDB_RS_TIDB_PORT=25000 \
//!     cargo test -p tidb-distsql --test cluster -- --nocapture
//! ```

use std::process::Command;

use tidb_distsql::AggFunc;
use tidb_distsql::DagContext;
use tidb_distsql::DagSpec;
use tidb_distsql::Datum;
use tidb_distsql::HandleRange;
use tidb_distsql::SelectOptions;
use tidb_distsql::build_dag;
use tidb_distsql::record_range;
use tidb_distsql::record_ranges;
use tidb_kv::KvClient;
use tidb_meta::InfoSchema;
use tidb_meta::MetaReader;
use tidb_meta::SchemaCache;
use tidb_meta::model::TableInfo;
use tidb_query_datatype::codec::mysql::Decimal;

const DB: &str = "tidb_rs_kv_it";
const ROWS: i64 = 20_000;

struct Env {
    pd: String,
    port: String,
}

fn env() -> Option<Env> {
    Some(Env {
        pd: std::env::var("TIDB_RS_PD").ok()?,
        port: std::env::var("TIDB_RS_TIDB_PORT").ok()?,
    })
}

impl Env {
    /// Run SQL in Go TiDB; tab-separated rows without header.
    fn sql(&self, sql: &str) -> String {
        let out = Command::new("mysql")
            .args([
                "-h",
                "127.0.0.1",
                "-P",
                &self.port,
                "-u",
                "root",
                "-N",
                "-B",
            ])
            .args(["-e", sql])
            .output()
            .expect("run mysql client");
        assert!(
            out.status.success(),
            "{sql}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    fn lines(&self, sql: &str) -> Vec<String> {
        self.sql(sql).lines().map(str::to_owned).collect()
    }
}

/// Render a datum the way the mysql client prints it in batch mode.
fn show(d: &Datum) -> String {
    match d {
        Datum::Null => "NULL".to_owned(),
        Datum::I64(v) => v.to_string(),
        Datum::U64(v) => v.to_string(),
        Datum::F64(v) => v.to_string(),
        Datum::Dec(v) => v.to_string(),
        Datum::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
        other => format!("{other:?}"),
    }
}

fn show_row(row: &[Datum]) -> String {
    row.iter().map(show).collect::<Vec<_>>().join("\t")
}

fn as_i128(d: &Datum) -> i128 {
    match d {
        Datum::I64(v) => i128::from(*v),
        Datum::U64(v) => i128::from(*v),
        Datum::Dec(v) => v.to_string().parse().unwrap(),
        Datum::Null => 0,
        other => panic!("not a number: {other:?}"),
    }
}

/// sysbench's `c`: 10 groups of 11 digits separated by '-' (deterministic here).
fn sb_c(i: i64) -> String {
    (0..10)
        .map(|g| format!("{:011}", (i * 7919 + g * 104_729) % 100_000_000_000))
        .collect::<Vec<_>>()
        .join("-")
}

fn sb_pad(i: i64) -> String {
    (0..5)
        .map(|g| format!("{:011}", (i * 31 + g * 13) % 100_000_000_000))
        .collect::<Vec<_>>()
        .join("-")
}

fn setup(env: &Env) {
    env.sql(&format!(
        "DROP DATABASE IF EXISTS {DB}; CREATE DATABASE {DB}; \
         CREATE TABLE {DB}.sbtest1(id INT NOT NULL AUTO_INCREMENT, k INT DEFAULT '0' NOT NULL, \
           c CHAR(120) DEFAULT '' NOT NULL, pad CHAR(60) DEFAULT '' NOT NULL, \
           PRIMARY KEY (id), KEY k_1(k)); \
         CREATE TABLE {DB}.parted(id INT PRIMARY KEY, v INT) PARTITION BY HASH(id) PARTITIONS 4;"
    ));
    for start in (1..=ROWS).step_by(2000) {
        let values: Vec<String> = (start..start + 2000)
            .map(|i| format!("({i},{},'{}','{}')", (i * 37) % 5000, sb_c(i), sb_pad(i)))
            .collect();
        env.sql(&format!(
            "INSERT INTO {DB}.sbtest1(id,k,c,pad) VALUES {}",
            values.join(",")
        ));
    }
    // Several regions, so scans and aggregations fan out.
    env.sql(&format!(
        "SPLIT TABLE {DB}.sbtest1 BETWEEN (0) AND ({ROWS}) REGIONS 8"
    ));
}

async fn info_schema(client: &KvClient, cache: &SchemaCache) -> (u64, std::sync::Arc<InfoSchema>) {
    let ts = client.current_ts().await.unwrap();
    (ts, cache.info_schema_at(ts).await.unwrap())
}

fn spec<'a>(t: &'a TableInfo, cols: &[&str]) -> DagSpec<'a> {
    DagSpec {
        table: t,
        columns: cols.iter().map(|c| t.find_column(c).unwrap()).collect(),
        desc: false,
        aggregation: None,
        limit: None,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn read_path_matches_go_tidb() {
    let Some(env) = env() else {
        eprintln!("TIDB_RS_PD / TIDB_RS_TIDB_PORT not set; skipping");
        return;
    };
    setup(&env);
    let client = KvClient::connect(vec![env.pd.clone()]).await.unwrap();
    let cache = SchemaCache::new(client.clone());
    let ctx = DagContext::select_utc();

    // --- Schema: load, find sbtest1, compare with Go's view. ---
    let (ts, is) = info_schema(&client, &cache).await;
    let t = is
        .table_by_name(DB, "SBTEST1")
        .expect("sbtest1 in InfoSchema");
    let go_id: i64 = env
        .sql(&format!(
            "SELECT TIDB_TABLE_ID FROM information_schema.tables \
             WHERE table_schema='{DB}' AND table_name='sbtest1'"
        ))
        .trim()
        .parse()
        .unwrap();
    assert_eq!(t.id, go_id);
    assert!(t.pk_is_handle);
    let names: Vec<_> = t
        .public_columns()
        .iter()
        .map(|c| c.name.o.clone())
        .collect();
    assert_eq!(names, ["id", "k", "c", "pad"]);
    assert!(t.find_index("k_1").is_some());
    let mut snap = client.snapshot(ts);
    let raw_version = MetaReader::new(&mut snap).schema_version().await.unwrap();
    println!(
        "schema version {} (version key {raw_version}), {} schemas, sbtest1 id {}",
        is.version(),
        is.schemas().count(),
        t.id
    );
    assert!(is.version() == raw_version || is.version() == raw_version - 1);
    // Every database Go lists is loaded (incl. mysql.* tables, which exercises the JSON
    // decoding of many real TableInfos).
    let go_dbs = env.lines(
        "SELECT LOWER(schema_name) FROM information_schema.schemata \
         WHERE schema_name NOT IN ('INFORMATION_SCHEMA','PERFORMANCE_SCHEMA','METRICS_SCHEMA','sys')",
    );
    for db in &go_dbs {
        assert!(is.schema_by_name(db).is_some(), "database {db} missing");
    }
    let go_mysql_tables: usize = env
        .sql("SELECT COUNT(*) FROM information_schema.tables WHERE table_schema='mysql' AND table_type='BASE TABLE'")
        .trim()
        .parse()
        .unwrap();
    let our_mysql_tables = is
        .tables("mysql")
        .iter()
        .filter(|t| !t.is_view() && !t.is_sequence())
        .count();
    assert_eq!(our_mysql_tables, go_mysql_tables);

    // Partitioned tables are detected and rejected.
    let parted = is.table_by_name(DB, "parted").unwrap();
    assert!(parted.is_partitioned());
    assert!(parted.check_int_handle_readable().is_err());

    // --- Point get by handle. ---
    let cols = t.public_columns();
    let mut snap = client.snapshot(client.current_ts().await.unwrap());
    for h in [1, 42, 12_345, ROWS] {
        let row = tidb_distsql::point_get(&mut snap, &t, h, &cols, &ctx)
            .await
            .unwrap()
            .unwrap();
        let go = env.sql(&format!("SELECT id,k,c,pad FROM {DB}.sbtest1 WHERE id={h}"));
        assert_eq!(show_row(&row), go.trim_end(), "row {h}");
        assert_eq!(row[2], Datum::Bytes(sb_c(h).into_bytes()));
    }
    assert!(
        tidb_distsql::point_get(&mut snap, &t, ROWS + 1, &cols, &ctx)
            .await
            .unwrap()
            .is_none()
    );
    let batch = tidb_distsql::batch_point_get(&mut snap, &t, &[5, ROWS + 7, 3], &cols, &ctx)
        .await
        .unwrap();
    assert_eq!(
        batch.iter().map(|r| as_i128(&r[0])).collect::<Vec<_>>(),
        [5, 3]
    );

    // --- Range scan through the coprocessor (sysbench simple_ranges is 100 rows). ---
    let plan = build_dag(&spec(&t, &["id", "k", "c", "pad"]), &ctx).unwrap();
    for (lo, hi) in [
        (1, 100),
        (2400, 2600),
        (1, ROWS),
        (ROWS - 5, ROWS + 100),
        (-10, 0),
    ] {
        let ranges = vec![record_range(t.id, HandleRange::inclusive(lo, hi)).unwrap()];
        let opts = SelectOptions {
            keep_order: true,
            schema_version: is.version(),
            ..SelectOptions::default()
        };
        let got = tidb_distsql::select(&mut snap, &plan, ranges, opts)
            .await
            .unwrap();
        let ours: Vec<String> = got.rows.iter().map(|r| show_row(r)).collect();
        let go = env.lines(&format!(
            "SELECT id,k,c,pad FROM {DB}.sbtest1 WHERE id BETWEEN {lo} AND {hi} ORDER BY id"
        ));
        println!(
            "scan [{lo}, {hi}]: {} rows (Go TiDB {})",
            ours.len(),
            go.len()
        );
        assert_eq!(ours, go, "scan [{lo}, {hi}]");
        assert!(got.warnings.is_empty());
    }

    // Descending scan with a pushed limit (ORDER BY id DESC LIMIT 10), paged.
    let mut desc = spec(&t, &["id", "k"]);
    desc.desc = true;
    desc.limit = Some(10);
    let plan = build_dag(&desc, &ctx).unwrap();
    let got = tidb_distsql::select(
        &mut snap,
        &plan,
        vec![record_range(t.id, HandleRange::full()).unwrap()],
        SelectOptions {
            keep_order: true,
            paging: Some((4, 64)),
            ..SelectOptions::default()
        },
    )
    .await
    .unwrap();
    // Each region applies the limit; the caller takes the first 10 in order.
    let ours: Vec<String> = got.rows.iter().take(10).map(|r| show_row(r)).collect();
    let go = env.lines(&format!(
        "SELECT id,k FROM {DB}.sbtest1 ORDER BY id DESC LIMIT 10"
    ));
    assert_eq!(ours, go);

    // --- Pushed partial aggregation: SUM(k), COUNT(k) over ranges (sum_ranges). ---
    let mut agg = spec(&t, &["k"]);
    agg.aggregation = Some((vec![AggFunc::Sum(0), AggFunc::Count(0)], vec![]));
    let plan = build_dag(&agg, &ctx).unwrap();
    let handle_ranges = [
        HandleRange::inclusive(10, 20),
        HandleRange::inclusive(2490, 2510),
        HandleRange::inclusive(7000, 15_000),
    ];
    for (name, ranges, cond) in [
        (
            "full",
            vec![record_range(t.id, HandleRange::full()).unwrap()],
            "1".to_owned(),
        ),
        (
            "multi-range",
            record_ranges(t.id, &handle_ranges),
            "id BETWEEN 10 AND 20 OR id BETWEEN 2490 AND 2510 OR id BETWEEN 7000 AND 15000"
                .to_owned(),
        ),
    ] {
        let got = tidb_distsql::select(&mut snap, &plan, ranges, SelectOptions::default())
            .await
            .unwrap();
        // Merge the per-region partial results (final SUM / COUNT).
        let mut sum = Decimal::zero();
        let mut count = 0i128;
        for row in &got.rows {
            if let Datum::Dec(d) = &row[0] {
                sum = (&sum + d).unwrap();
            }
            count += as_i128(&row[1]);
        }
        let go = env.sql(&format!(
            "SELECT SUM(k), COUNT(k) FROM {DB}.sbtest1 WHERE {cond}"
        ));
        println!(
            "agg {name}: SUM(k)={sum} COUNT(k)={count} from {} partials; Go TiDB: {}",
            got.rows.len(),
            go.trim_end()
        );
        assert_eq!(format!("{sum}\t{count}"), go.trim_end());
        assert!(got.rows.len() > 1, "expected several region partials");
    }

    // Group by k with COUNT and FIRST_ROW over a small range, merged per group.
    let mut grouped = spec(&t, &["id", "k"]);
    grouped.aggregation = Some((vec![AggFunc::Count(0)], vec![1]));
    let plan = build_dag(&grouped, &ctx).unwrap();
    let got = tidb_distsql::select(
        &mut snap,
        &plan,
        vec![record_range(t.id, HandleRange::inclusive(1, 6000)).unwrap()],
        SelectOptions::default(),
    )
    .await
    .unwrap();
    let mut groups = std::collections::BTreeMap::<i128, i128>::new();
    for row in &got.rows {
        *groups.entry(as_i128(&row[1])).or_default() += as_i128(&row[0]);
    }
    let ours: Vec<String> = groups.iter().map(|(k, c)| format!("{k}\t{c}")).collect();
    let go = env.lines(&format!(
        "SELECT k, COUNT(id) FROM {DB}.sbtest1 WHERE id BETWEEN 1 AND 6000 GROUP BY k ORDER BY k"
    ));
    assert_eq!(ours, go);

    // --- Schema change: ADD COLUMN in Go TiDB; the next statement sees it. ---
    let before = cache.stats();
    let (ts_old, is_old) = info_schema(&client, &cache).await;
    assert_eq!(is_old.version(), is.version());
    assert!(
        cache.stats().hits > before.hits,
        "unchanged version must hit"
    );
    env.sql(&format!(
        "ALTER TABLE {DB}.sbtest1 ADD COLUMN extra INT NOT NULL DEFAULT 5"
    ));
    let (_, is_new) = info_schema(&client, &cache).await;
    println!(
        "after ADD COLUMN: schema version {} -> {}, {:?}",
        is_old.version(),
        is_new.version(),
        cache.stats()
    );
    assert!(is_new.version() > is_old.version());
    let t_new = is_new.table_by_name(DB, "sbtest1").unwrap();
    let extra = t_new.find_column("extra").expect("new column visible");
    assert_eq!(extra.origin_default, serde_json::json!("5"));
    // A statement with the old start_ts still gets the old schema.
    let again_old = cache.info_schema_at(ts_old).await.unwrap();
    assert_eq!(again_old.version(), is_old.version());
    assert!(
        again_old
            .table_by_name(DB, "sbtest1")
            .unwrap()
            .find_column("extra")
            .is_none()
    );

    // Old rows have no value for `extra`: TiKV fills in the pushed origin default.
    env.sql(&format!("UPDATE {DB}.sbtest1 SET extra = 9 WHERE id = 3"));
    let (ts, is_new) = info_schema(&client, &cache).await;
    let t_new = is_new.table_by_name(DB, "sbtest1").unwrap();
    let plan = build_dag(&spec(&t_new, &["id", "extra"]), &ctx).unwrap();
    let mut snap = client.snapshot(ts);
    let got = tidb_distsql::select(
        &mut snap,
        &plan,
        vec![record_range(t_new.id, HandleRange::inclusive(1, 5)).unwrap()],
        SelectOptions {
            keep_order: true,
            ..SelectOptions::default()
        },
    )
    .await
    .unwrap();
    let ours: Vec<String> = got.rows.iter().map(|r| show_row(r)).collect();
    let go = env.lines(&format!(
        "SELECT id, extra FROM {DB}.sbtest1 WHERE id BETWEEN 1 AND 5 ORDER BY id"
    ));
    assert_eq!(ours, go);
    let cols: Vec<_> = ["id", "extra"]
        .iter()
        .map(|c| t_new.find_column(c).unwrap())
        .collect();
    let row = tidb_distsql::point_get(&mut snap, &t_new, 4, &cols, &ctx)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(show_row(&row), "4\t5");

    env.sql(&format!("DROP DATABASE {DB}"));
}
