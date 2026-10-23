//! DAG construction and response/row decoding, using TableInfo JSON captured from TiDB
//! v8.5.8 (`crates/meta/tests/fixtures`).

use protobuf::Message;
use tidb_distsql::AggFunc;
use tidb_distsql::DagContext;
use tidb_distsql::DagSpec;
use tidb_distsql::Datum;
use tidb_distsql::Error;
use tidb_distsql::build_dag;
use tidb_distsql::dag::push_down_flag;
use tidb_distsql::decode_row_value;
use tidb_distsql::decode_select_response;
use tidb_meta::model::TableInfo;
use tidb_meta::model::tp;
use tidb_query_datatype::codec::datum;
use tidb_query_datatype::codec::mysql::Decimal;
use tidb_query_datatype::codec::row::v2::encoder_for_test::Column;
use tidb_query_datatype::codec::row::v2::encoder_for_test::RowEncoder;
use tidb_query_datatype::expr::EvalContext;
use tipb::AggFunctionMode;
use tipb::DagRequest;
use tipb::EncodeType;
use tipb::ExecType;
use tipb::ExprType;

fn table(name: &str) -> TableInfo {
    let path = format!(
        "{}/../meta/tests/fixtures/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

#[test]
fn table_scan_of_sbtest1() {
    let t = table("sbtest1");
    let cols = t.public_columns();
    let plan = build_dag(
        &DagSpec {
            table: &t,
            columns: cols.clone(),
            desc: true,
            aggregation: None,
            limit: Some(10),
        },
        &DagContext::select_utc(),
    )
    .unwrap();

    // Round trip through protobuf bytes, as TiKV sees it.
    let mut req = DagRequest::default();
    req.merge_from_bytes(&plan.encode().unwrap()).unwrap();
    assert_eq!(req.get_encode_type(), EncodeType::TypeDefault);
    assert_eq!(req.get_output_offsets(), [0, 1, 2, 3]);
    assert_eq!(req.get_time_zone_name(), "UTC");
    assert_eq!(req.get_time_zone_offset(), 0);
    // TruncateAsWarning | InSelectStmt | OverflowAsWarning | IgnoreZeroInDate |
    // DividedByZeroAsWarning, as Go sets for a SELECT.
    assert_eq!(req.get_flags(), 2 | 32 | 64 | 128 | 256);
    assert_eq!(
        req.get_flags() & push_down_flag::IN_SELECT_STMT,
        push_down_flag::IN_SELECT_STMT
    );

    let execs = req.get_executors();
    assert_eq!(execs.len(), 2);
    assert_eq!(execs[0].get_tp(), ExecType::TypeTableScan);
    assert_eq!(execs[1].get_tp(), ExecType::TypeLimit);
    assert_eq!(execs[1].get_limit().get_limit(), 10);
    let scan = execs[0].get_tbl_scan();
    assert_eq!(scan.get_table_id(), t.id);
    assert!(scan.get_desc());
    let pb: Vec<_> = scan
        .get_columns()
        .iter()
        .map(|c| {
            (
                c.get_column_id(),
                c.get_tp(),
                c.get_pk_handle(),
                c.get_collation(),
                c.get_column_len(),
            )
        })
        .collect();
    // id INT PK (handle), k INT, c CHAR(120) utf8mb4_bin, pad CHAR(60); collation ids are
    // negated (new collation).
    assert_eq!(
        pb,
        [
            (1, 3, true, -63, 11),
            (2, 3, false, -63, 11),
            (3, 254, false, -46, 120),
            (4, 254, false, -46, 60),
        ]
    );
    assert!(scan.get_columns().iter().all(|c| !c.has_default_val()));
    assert_eq!(plan.output_types.len(), 4);
}

#[test]
fn partial_aggregation() {
    let t = table("sbtest1");
    let k = t.find_column("k").unwrap();
    let c = t.find_column("c").unwrap();
    let plan = build_dag(
        &DagSpec {
            table: &t,
            columns: vec![k, c],
            desc: false,
            aggregation: Some((
                vec![AggFunc::Sum(0), AggFunc::Count(0), AggFunc::FirstRow(1)],
                vec![1],
            )),
            limit: None,
        },
        &DagContext::select_utc(),
    )
    .unwrap();
    let execs = plan.request.get_executors();
    assert_eq!(execs[1].get_tp(), ExecType::TypeAggregation);
    let agg = execs[1].get_aggregation();
    let funcs = agg.get_agg_func();
    assert_eq!(
        funcs.iter().map(|f| f.get_tp()).collect::<Vec<_>>(),
        [ExprType::Sum, ExprType::Count, ExprType::First]
    );
    assert!(
        funcs
            .iter()
            .all(|f| f.get_agg_func_mode() == AggFunctionMode::Partial1Mode)
    );
    // SUM(INT(11)) is DECIMAL(32, 0); COUNT is BIGINT(21) NOT NULL.
    let sum_ft = funcs[0].get_field_type();
    assert_eq!(
        (sum_ft.get_tp(), sum_ft.get_flen(), sum_ft.get_decimal()),
        (246, 32, 0)
    );
    let count_ft = funcs[1].get_field_type();
    assert_eq!((count_ft.get_tp(), count_ft.get_flen()), (8, 21));
    assert_eq!(count_ft.get_flag() & 1, 1);
    // Group by c: a column reference to scan offset 1 with c's collation.
    let g = &agg.get_group_by()[0];
    assert_eq!(g.get_tp(), ExprType::ColumnRef);
    assert_eq!(g.get_field_type().get_collate(), -46);
    // Output: 3 aggregate results then the group-by column.
    assert_eq!(plan.request.get_output_offsets(), [0, 1, 2, 3]);
    let tps: Vec<_> = plan.output_types.iter().map(|f| f.tp).collect();
    assert_eq!(tps, [tp::NEWDECIMAL, tp::LONGLONG, tp::STRING, tp::STRING]);
}

#[test]
fn origin_defaults_are_pushed_down() {
    let t = table("fixture_misc");
    let g = t.find_column("g").unwrap();
    let h = t.find_column("h").unwrap();
    let ctx = DagContext {
        new_collation_enabled: false,
        ..DagContext::select_utc()
    };
    let plan = build_dag(
        &DagSpec {
            table: &t,
            columns: vec![g, h],
            desc: false,
            aggregation: None,
            limit: None,
        },
        &ctx,
    )
    .unwrap();
    let cols = plan.request.get_executors()[0].get_tbl_scan().get_columns();
    let mut ectx = EvalContext::default();
    // Go: tablecodec.EncodeValue(origin default cast to the column type).
    assert_eq!(
        cols[0].get_default_val(),
        datum::encode_value(&mut ectx, &[Datum::I64(7)]).unwrap()
    );
    assert_eq!(
        cols[1].get_default_val(),
        datum::encode_value(&mut ectx, &[Datum::Bytes(b"hi".to_vec())]).unwrap()
    );
    // Without new collations the ids are positive.
    assert_eq!(cols[1].get_collation(), 46);
    // `_tidb_rowid` table: no column is the pk handle.
    assert!(!cols.iter().any(|c| c.get_pk_handle()));
}

#[test]
fn unsupported_tables_and_columns() {
    let part = table("fixture_part");
    let err = build_dag(
        &DagSpec {
            table: &part,
            columns: part.public_columns(),
            desc: false,
            aggregation: None,
            limit: None,
        },
        &DagContext::select_utc(),
    )
    .unwrap_err();
    assert!(
        matches!(&err, Error::Unsupported(m) if m.contains("partitioned")),
        "{err}"
    );

    // DATETIME DEFAULT CURRENT_TIMESTAMP has no origin default, so it is fine; an origin
    // default of an unsupported type is rejected rather than silently dropped.
    let mut misc = table("fixture_misc");
    let d = misc.columns.iter_mut().find(|c| c.name.l == "d").unwrap();
    d.origin_default = serde_json::json!("2024-01-01 00:00:00.000");
    let d = misc.find_column("d").unwrap();
    let err = build_dag(
        &DagSpec {
            table: &misc,
            columns: vec![d],
            desc: false,
            aggregation: None,
            limit: None,
        },
        &DagContext::select_utc(),
    )
    .unwrap_err();
    assert!(matches!(err, Error::Unsupported(_)), "{err}");
}

/// A `SelectResponse` as TiKV encodes it with `TypeDefault`: datum-encoded rows, split over
/// chunks.
#[test]
fn decode_select_response_rows() {
    let t = table("sbtest1");
    let types: Vec<_> = [t.find_column("id"), t.find_column("c")]
        .iter()
        .map(|c| c.unwrap().field_type.clone())
        .collect();
    let mut ctx = EvalContext::default();
    let mut resp = tipb::SelectResponse::default();
    for rows in [&[(1i64, "a"), (2, "bb")][..], &[(3, "")][..]] {
        let mut data = Vec::new();
        for (id, c) in rows {
            data.extend(
                datum::encode_value(
                    &mut ctx,
                    &[Datum::I64(*id), Datum::Bytes(c.as_bytes().to_vec())],
                )
                .unwrap(),
            );
        }
        let mut chunk = tipb::Chunk::default();
        chunk.set_rows_data(data);
        resp.mut_chunks().push(chunk);
    }
    let rows = decode_select_response(&resp.write_to_bytes().unwrap(), &types).unwrap();
    assert_eq!(
        rows.rows,
        [
            vec![Datum::I64(1), Datum::Bytes(b"a".to_vec())],
            vec![Datum::I64(2), Datum::Bytes(b"bb".to_vec())],
            vec![Datum::I64(3), Datum::Bytes(vec![])],
        ]
    );

    // An error in the response is reported.
    let mut err = tipb::SelectResponse::default();
    err.mut_error().set_code(1105);
    err.mut_error().set_msg("boom".to_owned());
    assert!(matches!(
        decode_select_response(&err.write_to_bytes().unwrap(), &types),
        Err(Error::Coprocessor { code: 1105, .. })
    ));

    // A partial aggregation result: DECIMAL sum and BIGINT count.
    let mut agg = tipb::SelectResponse::default();
    let mut chunk = tipb::Chunk::default();
    chunk.set_rows_data(
        datum::encode_value(
            &mut ctx,
            &[
                Datum::Dec("12345".parse::<Decimal>().unwrap()),
                Datum::I64(7),
            ],
        )
        .unwrap(),
    );
    agg.mut_chunks().push(chunk);
    let k = t.find_column("k").unwrap();
    let types = [
        AggFunc::Sum(0).ret_type(&k.field_type),
        AggFunc::Count(0).ret_type(&k.field_type),
    ];
    let rows = decode_select_response(&agg.write_to_bytes().unwrap(), &types).unwrap();
    assert_eq!(
        rows.rows,
        [vec![Datum::Dec("12345".parse().unwrap()), Datum::I64(7)]]
    );
}

/// Row format v2 values as TiDB writes them, including a column added later (missing from
/// the row, filled from its origin default) and a NULL.
#[test]
fn decode_row_v2() {
    let t = table("fixture_misc");
    let mut ctx = EvalContext::default();
    let mut value = vec![];
    value
        .write_row(
            &mut ctx,
            vec![
                Column::new(1, 42i64),                 // a BIGINT
                Column::new(3, Some(b"xyz".to_vec())), // c VARCHAR
                Column::new(6, None::<i64>),           // f DOUBLE NULL
            ],
        )
        .unwrap();
    let names = ["a", "c", "f", "g", "h", "b"];
    let cols: Vec<_> = names.iter().map(|n| t.find_column(n).unwrap()).collect();
    let row = decode_row_value(&t, 99, &value, &cols, &DagContext::select_utc()).unwrap();
    assert_eq!(
        row,
        [
            Datum::I64(42),
            Datum::Bytes(b"xyz".to_vec()),
            Datum::Null,
            Datum::I64(7),                // g: origin default
            Datum::Bytes(b"hi".to_vec()), // h: origin default
            Datum::Null,                  // b: absent, no origin default
        ]
    );

    // pk_is_handle: the primary key comes from the handle, not the value.
    let sb = table("sbtest1");
    let mut value = vec![];
    value
        .write_row(
            &mut ctx,
            vec![
                Column::new(2, 5i64),
                Column::new(3, b"c-value".to_vec()),
                Column::new(4, b"pad".to_vec()),
            ],
        )
        .unwrap();
    let cols = sb.public_columns();
    let row = decode_row_value(&sb, 1234, &value, &cols, &DagContext::select_utc()).unwrap();
    assert_eq!(
        row,
        [
            Datum::I64(1234),
            Datum::I64(5),
            Datum::Bytes(b"c-value".to_vec()),
            Datum::Bytes(b"pad".to_vec()),
        ]
    );
}

#[test]
fn decode_row_v1() {
    let sb = table("sbtest1");
    let mut ctx = EvalContext::default();
    // v1: (column id, value) datum pairs.
    let value = datum::encode_value(
        &mut ctx,
        &[
            Datum::I64(2),
            Datum::I64(5),
            Datum::I64(3),
            Datum::Bytes(b"old".to_vec()),
        ],
    )
    .unwrap();
    let cols = sb.public_columns();
    let row = decode_row_value(&sb, 1, &value, &cols, &DagContext::select_utc()).unwrap();
    assert_eq!(
        row,
        [
            Datum::I64(1),
            Datum::I64(5),
            Datum::Bytes(b"old".to_vec()),
            Datum::Null,
        ]
    );
}
