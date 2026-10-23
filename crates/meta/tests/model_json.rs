//! Decoding of TableInfo / DBInfo JSON captured from a TiDB v8.5.8 cluster (via the status
//! API `/schema/{db}/{table}`, which marshals the same `model.TableInfo` as the meta).
//!
//! Fixtures:
//! - `sbtest1`: sysbench table (`id INT AUTO_INCREMENT PRIMARY KEY, k INT, c CHAR(120),
//!   pad CHAR(60), KEY k_1(k)`).
//! - `fixture_misc`: non-clustered BIGINT PK, decimal/varchar/datetime/enum/double columns,
//!   and `g INT NOT NULL DEFAULT 7`, `h VARCHAR(10) DEFAULT 'hi'` added by ALTER TABLE.
//! - `fixture_part`: `PARTITION BY HASH(id) PARTITIONS 4`.

use tidb_meta::model::DbInfo;
use tidb_meta::model::SchemaState;
use tidb_meta::model::TableInfo;
use tidb_meta::model::Unsupported;
use tidb_meta::model::flag;
use tidb_meta::model::tp;

fn table(name: &str) -> TableInfo {
    let path = format!("{}/tests/fixtures/{name}.json", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

#[test]
fn sbtest1() {
    let t = table("sbtest1");
    assert_eq!(t.name.o, "sbtest1");
    assert!(t.pk_is_handle);
    assert!(!t.is_common_handle);
    assert!(!t.is_partitioned());
    assert_eq!(t.state, SchemaState::Public);
    assert_eq!(t.check_int_handle_readable(), Ok(()));

    let cols: Vec<_> = t
        .public_columns()
        .iter()
        .map(|c| (c.id, c.name.o.as_str(), c.field_type.tp, c.field_type.flen))
        .collect();
    assert_eq!(
        cols,
        [
            (1, "id", tp::LONG, 11),
            (2, "k", tp::LONG, 11),
            (3, "c", tp::STRING, 120),
            (4, "pad", tp::STRING, 60),
        ]
    );

    let id = t.pk_handle_column().unwrap();
    assert_eq!(id.name.l, "id");
    let f = &id.field_type;
    assert!(f.has_flag(flag::PRI_KEY) && f.has_flag(flag::NOT_NULL));
    assert!(f.has_flag(flag::AUTO_INCREMENT));
    assert_eq!(
        (f.charset.as_str(), f.collate.as_str()),
        ("binary", "binary")
    );

    let c = t.find_column("C").unwrap();
    assert_eq!(c.field_type.charset, "utf8mb4");
    assert_eq!(c.field_type.collate, "utf8mb4_bin");
    assert!(c.field_type.is_not_null());
    assert_eq!(c.default_value, serde_json::json!(""));
    assert!(c.origin_default.is_null());

    let k_1 = t.find_index("K_1").unwrap();
    assert!(!k_1.unique && !k_1.primary);
    assert_eq!(k_1.columns.len(), 1);
    assert_eq!(k_1.columns[0].name.l, "k");
    assert_eq!(k_1.columns[0].offset, 1);
    assert_eq!(k_1.columns[0].length, -1);
}

#[test]
fn misc_types_and_added_columns() {
    let t = table("fixture_misc");
    // NONCLUSTERED: rows are keyed by the hidden _tidb_rowid; still an int handle.
    assert!(!t.pk_is_handle);
    assert!(!t.is_common_handle);
    assert!(t.pk_handle_column().is_none());
    assert_eq!(t.check_int_handle_readable(), Ok(()));
    assert!(t.find_index("primary").unwrap().primary);

    let tps: Vec<_> = t.public_columns().iter().map(|c| c.field_type.tp).collect();
    assert_eq!(
        tps,
        [
            tp::LONGLONG,
            tp::NEWDECIMAL,
            tp::VARCHAR,
            tp::DATETIME,
            tp::ENUM,
            tp::DOUBLE,
            tp::LONG,
            tp::VARCHAR,
        ]
    );
    let b = t.find_column("b").unwrap();
    assert_eq!((b.field_type.flen, b.field_type.decimal), (10, 2));
    let d = t.find_column("d").unwrap();
    assert_eq!(d.field_type.decimal, 3);
    assert_eq!(d.default_value, serde_json::json!("CURRENT_TIMESTAMP"));
    let e = t.find_column("e").unwrap();
    assert_eq!(e.field_type.elems, ["x", "y"]);

    // Columns added after creation carry the origin default for older rows.
    let g = t.find_column("g").unwrap();
    assert_eq!(g.origin_default, serde_json::json!("7"));
    assert!(g.field_type.is_not_null());
    let h = t.find_column("h").unwrap();
    assert_eq!(h.origin_default, serde_json::json!("hi"));
    assert_eq!(t.max_col_id, 8);
}

#[test]
fn partitioned_table_is_rejected() {
    let t = table("fixture_part");
    assert!(t.is_partitioned());
    assert!(t.pk_is_handle);
    assert_eq!(
        t.check_int_handle_readable(),
        Err(Unsupported::Partitioned("fixture_part".to_owned()))
    );
}

#[test]
fn db_infos() {
    let path = format!("{}/tests/fixtures/dbs.json", env!("CARGO_MANIFEST_DIR"));
    let dbs: Vec<DbInfo> = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let mut names: Vec<_> = dbs.iter().map(|d| d.name.o.as_str()).collect();
    names.sort();
    assert_eq!(names, ["mysql", "sbtest"]);
    assert!(
        dbs.iter()
            .all(|d| d.state == SchemaState::Public && d.id > 0)
    );
    let sbtest = dbs.iter().find(|d| d.name.l == "sbtest").unwrap();
    assert_eq!(sbtest.charset, "utf8mb4");
}

#[test]
fn robust_to_unknown_fields_nulls_and_states() {
    let t: TableInfo = serde_json::from_str(
        r#"{"id": 5, "name": {"O": "T", "L": "t"}, "cols": null, "index_info": null,
            "state": 9, "some_future_field": {"x": [1, 2]}, "partition": null}"#,
    )
    .unwrap();
    assert_eq!(t.id, 5);
    assert!(t.columns.is_empty() && t.indices.is_empty());
    assert_eq!(t.state, SchemaState::Unknown(9));
    assert!(!t.is_partitioned());
    assert!(matches!(
        t.check_int_handle_readable(),
        Err(Unsupported::NotPublic(..))
    ));
    // A view is not readable either.
    let v: TableInfo = serde_json::from_str(
        r#"{"id": 6, "name": {"O": "v", "L": "v"}, "state": 5, "view": {"view_select": "select 1"}}"#,
    )
    .unwrap();
    assert_eq!(
        v.check_int_handle_readable(),
        Err(Unsupported::View("v".to_owned()))
    );
}
