//! Rust counterparts of the Go benchmarks of `pkg/util/chunk` (chunk_test.go, codec_test.go,
//! chunk_util_test.go, list_test.go), same workloads. Go numbers: specs/D01.7-chunk/plan.md.

use std::hint::black_box;

use bytes::Bytes;
use criterion::Criterion;
use criterion::criterion_group;
use criterion::criterion_main;
use tidb_base::mysql;
use tidb_chunk::RowPtr;
use tidb_chunk::chunk;
use tidb_chunk::chunk_util;
use tidb_chunk::codec;
use tidb_chunk::list;
use tidb_parser::types::FieldType;
use tidb_types::json::BinaryJSON;
use tidb_types::new_dec_from_string_for_test;
use tidb_types::time::Duration;
use tidb_types::time::Time;

#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

fn ft(tp: u8) -> FieldType {
    FieldType::new(tp)
}

fn bench(c: &mut Criterion) {
    // BenchmarkAppendInt: Reset + 1000 AppendInt64 on a fixed column with no initial capacity.
    {
        let mut chk = chunk::new(&[ft(mysql::TypeLonglong)], 0, 1024);
        c.bench_function("AppendInt", |b| {
            b.iter(|| {
                chk.reset();
                for i in 0..1000 {
                    chk.append_int64(0, i);
                }
            })
        });
    }
    // BenchmarkAppendString.
    {
        let mut chk = chunk::new(&[ft(mysql::TypeVarchar)], 0, 1024);
        c.bench_function("AppendString", |b| {
            b.iter(|| {
                chk.reset();
                for _ in 0..1000 {
                    chk.append_string(0, b"abcd");
                }
            })
        });
    }
    // BenchmarkAppendRow: rows of (null int, 1, "abcd", "abcd").
    {
        let fts = [
            ft(mysql::TypeLonglong),
            ft(mysql::TypeLonglong),
            ft(mysql::TypeVarchar),
            ft(mysql::TypeBlob),
        ];
        let mut row_chk = chunk::new(&fts, 0, 1024);
        row_chk.append_null(0);
        row_chk.append_int64(1, 1);
        row_chk.append_string(2, b"abcd");
        row_chk.append_bytes(3, b"abcd");
        let mut chk = chunk::new(&fts, 0, 1024);
        c.bench_function("AppendRow", |b| {
            b.iter(|| {
                chk.reset();
                for _ in 0..1000 {
                    chk.append_row(row_chk.get_row(0));
                }
            })
        });
    }
    // BenchmarkAccess: 8192 GetRow(j).GetInt64(0).
    {
        let mut row_chk = chunk::new(&[ft(mysql::TypeLonglong)], 0, 8192);
        for _ in 0..8192 {
            row_chk.append_int64(0, u16::MAX as i64);
        }
        c.bench_function("Access", |b| {
            b.iter(|| {
                let mut sum = 0i64;
                for j in 0..8192 {
                    sum = sum.wrapping_add(row_chk.get_row(j).get_int64(0));
                }
                black_box(sum)
            })
        });
    }
    // BenchmarkEncodeChunk: 4 empty BIGINT columns with capacity 1024.
    {
        let fts = vec![ft(mysql::TypeLonglong); 4];
        let chk = chunk::new_chunk_with_capacity(&fts, 1024);
        let codec = codec::new_codec(Vec::new());
        c.bench_function("EncodeChunk", |b| b.iter(|| black_box(codec.encode(&chk))));
    }
    // BenchmarkDecode.
    {
        let fts = vec![ft(mysql::TypeLonglong); 4];
        let chk = chunk::new_chunk_with_capacity(&fts, 1024);
        let codec = codec::new_codec(fts);
        let buffer = Bytes::from(codec.encode(&chk));
        c.bench_function("Decode", |b| {
            b.iter(|| black_box(codec.decode(buffer.clone())))
        });
    }
    // BenchmarkDecodeToChunk: 4 BIGINT columns of 1024 rows. Go builds the columns with a
    // zeroed null bitmap and zeroed data, i.e. 1024 NULLs per column (nullCount > 0, so the
    // decoder slices the bitmap from the buffer); 1024 AppendNull give the same encoding.
    {
        let fts = vec![ft(mysql::TypeLonglong); 4];
        let mut chk = chunk::new_chunk_with_capacity(&fts, 1024);
        for _ in 0..1024 {
            for col in 0..4 {
                chk.append_null(col);
            }
        }
        let codec = codec::new_codec(fts);
        let buffer = Bytes::from(codec.encode(&chk));
        c.bench_function("DecodeToChunk", |b| {
            b.iter(|| black_box(codec.decode_to_chunk(buffer.clone(), &mut chk)))
        });
    }
    // BenchmarkDecodeToChunkWithVariableType.
    {
        let fts = vec![
            ft(mysql::TypeLonglong),
            ft(mysql::TypeLonglong),
            ft(mysql::TypeVarchar),
            ft(mysql::TypeVarchar),
            ft(mysql::TypeNewDecimal),
            ft(mysql::TypeJSON),
        ];
        let mut chk = chunk::new_chunk_with_capacity(&fts, 1024);
        for i in 0..1024 {
            let s = format!("{i}.12345");
            chk.append_null(0);
            chk.append_int64(1, i);
            chk.append_string(2, s.as_bytes());
            chk.append_string(3, s.as_bytes());
            chk.append_my_decimal(4, &new_dec_from_string_for_test(&s));
            // types.CreateBinaryJSON(str): a JSON string.
            let mut v = Vec::new();
            let n = s.len() as u64;
            let mut x = n;
            loop {
                let mut byte = (x & 0x7f) as u8;
                x >>= 7;
                if x != 0 {
                    byte |= 0x80;
                }
                v.push(byte);
                if x == 0 {
                    break;
                }
            }
            v.extend_from_slice(s.as_bytes());
            chk.append_json(
                4 + 1,
                &BinaryJSON {
                    type_code: tidb_types::json::JSONTypeCodeString,
                    value: v.into(),
                },
            );
        }
        let codec = codec::new_codec(fts);
        let buffer = Bytes::from(codec.encode(&chk));
        chk.reset();
        c.bench_function("DecodeToChunkWithVariableType", |b| {
            b.iter(|| black_box(codec.decode_to_chunk(buffer.clone(), &mut chk)))
        });
    }
    // BenchmarkCopySelectedJoinRowsDirect.
    {
        let fts = [
            ft(mysql::TypeVarchar),
            ft(mysql::TypeVarchar),
            ft(mysql::TypeLonglong),
            ft(mysql::TypeLonglong),
            ft(mysql::TypeDatetime),
            ft(mysql::TypeVarchar),
        ];
        let mut src = chunk::new_chunk_with_capacity(&fts, 1024);
        let mut selected = vec![false; 1024];
        for (j, sel) in selected.iter_mut().enumerate() {
            if j % 7 == 0 {
                src.append_string(0, b"abc");
                src.append_string(1, b"abcdefg");
                src.append_null(2);
                src.append_int64(3, (j as i64).wrapping_mul(2654435761));
            } else {
                src.append_string(0, b"aabc");
                src.append_string(1, b"ab234fg");
                src.append_int64(2, j as i64);
                src.append_int64(3, 123);
                *sel = true;
            }
            src.append_time(4, Time::default());
            src.append_string(5, b"abcdefg");
        }
        let mut dst = chunk::new_chunk_with_capacity(&fts, 1024);
        c.bench_function("CopySelectedJoinRowsDirect", |b| {
            b.iter(|| {
                dst.reset();
                chunk_util::copy_selected_join_rows_direct(&src, &selected, &mut dst).unwrap();
            })
        });
    }
    // BenchmarkListGetRow: 10000 chunks of 2 rows.
    {
        let fts = vec![
            ft(mysql::TypeVarString),
            ft(mysql::TypeLonglong),
            ft(mysql::TypeVarchar),
            ft(mysql::TypeLonglong),
            ft(mysql::TypeJSON),
        ];
        let mut l = list::new_list(fts.clone(), 2, 2);
        for i in 0..10000 {
            let mut chk = chunk::new_chunk_with_capacity(&fts, 2);
            for j in 0..2i64 {
                chk.append_string(0, b"abc");
                chk.append_int64(1, i + j);
                chk.append_string(2, b"def");
                chk.append_int64(3, j);
                chk.append_null(4);
            }
            l.add(chk).unwrap();
        }
        let mut x: u32 = 1;
        let ptrs: Vec<RowPtr> = (0..10000)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                RowPtr {
                    chk_idx: x % 10000,
                    row_idx: (x >> 16) % 2,
                }
            })
            .collect();
        // Go walks `ptrs` sequentially (the 10000 pointers repeated) and reports the time per
        // GetRow. One criterion iteration walks all 10000; divide the time by 10000.
        c.bench_function("ListGetRow (x10000)", |b| {
            b.iter(|| {
                for &p in &ptrs {
                    black_box(l.get_row(p));
                }
            })
        });
    }
    // BenchmarkChunkMemoryUsage.
    {
        let fts = [
            ft(mysql::TypeFloat),
            ft(mysql::TypeVarchar),
            ft(mysql::TypeDatetime),
            ft(mysql::TypeDuration),
        ];
        let mut chk = chunk::new_chunk_with_capacity(&fts, 10);
        for _ in 0..10 {
            chk.append_float64(0, 123.123);
            chk.append_string(1, b"123");
            chk.append_time(2, Time::default());
            chk.append_duration(
                3,
                Duration {
                    duration: i64::MAX,
                    fsp: 0,
                },
            );
        }
        c.bench_function("ChunkMemoryUsage", |b| {
            b.iter(|| black_box(chk.memory_usage()))
        });
    }
}

criterion_group!(benches, bench);
criterion_main!(benches);
