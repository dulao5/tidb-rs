//! D01.11 benchmarks, the same workloads and inputs as the Go side in tidb-rs-tracker
//! `govec/domains/tablecodec_bench_test.go` (inputs printed there by
//! `GOVEC_PRINT_BENCH=1 go test -run TestTablecodecBenchInputs -v ./domains/`).
//!
//! `cargo bench -p tidb-tablecodec`

use std::collections::HashMap;
use std::hint::black_box;
use std::sync::Arc;

use criterion::{Criterion, criterion_group, criterion_main};
use tidb_base::gostd::time::utc;
use tidb_base::mysql;
use tidb_chunk::chunk::new_chunk_with_capacity;
use tidb_kv::key::{Handle, IntHandle};
use tidb_model::ColumnInfo;
use tidb_parser::types::FieldType;
use tidb_tablecodec as tc;
use tidb_tablecodec::rowcodec::{self, ColData, ColInfo, RowData};
use tidb_types::collate;
use tidb_types::datum as dm;
use tidb_types::field_type::new_field_type;

#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

const ROW3: &str = "8000030000000203ff03000b000c00616263bff199999999999a01";
const ROW10: &str = "80000a0000000102030405060708090a010002000a0015001e0023002b0033003b004c002af9c00a00000000000068656c6c6f20776f726c645469646220527573740502807b2d00000009724cb31900ae17d4620300000000000000010000736f6d6520626c6f62207061796c6f6164";
const ROW_V1: &str = "080208540804080d080605c00a0000000000000808021668656c6c6f20776f726c64080a0212546964622052757374080c060502807b2d080e09808080c8a08ed3d91908100880b8bdc1dad80108120880808080804008140222736f6d6520626c6f62207061796c6f6164";
const IDX_INT_VAL: &str = "0000000000000100";
const IDX_COMMON_VAL: &str = "007d017f0012038000000000000001038000000000000002";
const IDX_KEY: &str = "7480000000000000645f6980000000000000020100480045004c004cff004f00200057004fff0052004c00440000fd038000000000003039";
const IDX_VAL: &str = "00800001000000010b0048656c6c6f20576f726c64";

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

fn fts10() -> Vec<FieldType> {
    let mut fts: Vec<FieldType> = [
        mysql::TypeLonglong,
        mysql::TypeLong,
        mysql::TypeDouble,
        mysql::TypeVarchar,
        mysql::TypeVarchar,
        mysql::TypeNewDecimal,
        mysql::TypeDatetime,
        mysql::TypeDuration,
        mysql::TypeLonglong,
        mysql::TypeBlob,
    ]
    .into_iter()
    .map(new_field_type)
    .collect();
    fts[3].set_collate("utf8mb4_bin");
    fts[4].set_collate("utf8mb4_general_ci");
    fts[5].set_flen(10);
    fts[5].set_decimal(2);
    fts
}

fn general_ci_varchar() -> FieldType {
    let mut ft = new_field_type(mysql::TypeVarchar);
    ft.set_charset("utf8mb4");
    ft.set_collate("utf8mb4_general_ci");
    ft
}

fn col(id: i64, ft: &FieldType) -> ColInfo {
    ColInfo {
        id,
        is_pk_handle: false,
        virtual_gen_col: false,
        ft: Arc::new(ft.clone()),
    }
}

fn bench(c: &mut Criterion) {
    collate::set_new_collation_enabled_for_test(true);
    let loc = Some(utc());

    c.bench_function("EncodeRowKeyWithHandle", |b| {
        let h = Handle::Int(IntHandle(100));
        b.iter(|| black_box(tc::encode_row_key_with_handle(black_box(100), &h)))
    });

    c.bench_function("DecodeRowKey", |b| {
        let k = tc::encode_row_key_with_handle(100, &Handle::Int(IntHandle(100)));
        b.iter(|| black_box(tc::decode_row_key(black_box(&k))))
    });

    c.bench_function("DecodeHandleInIndexValueInt", |b| {
        let v = unhex(IDX_INT_VAL);
        b.iter(|| black_box(tc::decode_handle_in_index_value(black_box(&v))))
    });

    c.bench_function("DecodeHandleInIndexValueCommon", |b| {
        let v = unhex(IDX_COMMON_VAL);
        b.iter(|| black_box(tc::decode_handle_in_index_value(black_box(&v))))
    });

    c.bench_function("DecodeToChunk3", |b| {
        let row = unhex(ROW3);
        let tps = [
            new_field_type(mysql::TypeLonglong),
            new_field_type(mysql::TypeString),
            new_field_type(mysql::TypeDouble),
        ];
        let cols = vec![col(-1, &tps[0]), col(2, &tps[1]), col(3, &tps[2])];
        let d = rowcodec::new_chunk_decoder(cols, vec![-1], None, loc.clone());
        let mut chk = new_chunk_with_capacity(&tps, 1);
        let h = Handle::Int(IntHandle(1));
        b.iter(|| {
            chk.reset();
            black_box(d.decode_to_chunk(black_box(&row), 0, Some(&h), &mut chk)).unwrap();
        })
    });

    c.bench_function("DecodeToChunk10", |b| {
        let row = unhex(ROW10);
        let fts = fts10();
        let cols = fts
            .iter()
            .enumerate()
            .map(|(i, ft)| col(i as i64 + 1, ft))
            .collect();
        let d = rowcodec::new_chunk_decoder(cols, Vec::new(), None, loc.clone());
        let mut chk = new_chunk_with_capacity(&fts, 1024);
        let mut i = 0usize;
        b.iter(|| {
            if i % 1024 == 0 {
                chk.reset();
            }
            i += 1;
            black_box(d.decode_to_chunk(black_box(&row), 0, None, &mut chk)).unwrap();
        })
    });

    c.bench_function("DecodeToDatumMap10", |b| {
        let row = unhex(ROW10);
        let cols: HashMap<i64, Arc<FieldType>> = fts10()
            .into_iter()
            .enumerate()
            .map(|(i, ft)| (i as i64 + 1, Arc::new(ft)))
            .collect();
        b.iter(|| {
            black_box(tc::decode_row_to_datum_map(
                black_box(&row),
                &cols,
                loc.as_ref(),
            ))
            .unwrap()
        })
    });

    c.bench_function("CutRowNewV1", |b| {
        let row = unhex(ROW_V1);
        let ids: HashMap<i64, usize> = (0..10).map(|i| (i as i64 + 1, i)).collect();
        b.iter(|| black_box(tc::cut_row_new(black_box(&row), &ids)).unwrap())
    });

    c.bench_function("DecodeIndexKVRestored", |b| {
        let key = unhex(IDX_KEY);
        let val = unhex(IDX_VAL);
        let cols = vec![col(1, &general_ci_varchar())];
        b.iter(|| {
            black_box(tc::decode_index_kv(
                black_box(&key),
                black_box(&val),
                1,
                tc::HandleStatus::HandleDefault,
                &cols,
            ))
            .unwrap()
        })
    });

    c.bench_function("Checksum3", |b| {
        let datums = [
            dm::new_int_datum(1),
            dm::new_string_datum("abc"),
            dm::new_float64_datum(1.1),
        ];
        let infos: Vec<ColumnInfo> = [
            (1, mysql::TypeLong),
            (2, mysql::TypeVarchar),
            (3, mysql::TypeDouble),
        ]
        .into_iter()
        .map(|(id, tp)| ColumnInfo {
            id,
            field_type: new_field_type(tp),
            ..ColumnInfo::default()
        })
        .collect();
        let mut row = RowData {
            cols: infos
                .iter()
                .zip(&datums)
                .map(|(ci, d)| ColData {
                    column_info: ci,
                    datum: d,
                })
                .collect(),
            data: Vec::new(),
        };
        b.iter(|| black_box(row.checksum(loc.as_ref())).unwrap())
    });
}

criterion_group!(benches, bench);
criterion_main!(benches);
