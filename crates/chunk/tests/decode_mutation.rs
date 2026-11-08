//! Decoding malformed chunk bytes (a TiKV response crosses a process boundary): truncations,
//! byte flips and overwritten lengths / null counts must end in a catchable panic or a decoded
//! chunk, never an abort or a hang. Go's decoder also panics on malformed input (Lucky's D01.7
//! review ran 200,000 copies; this keeps a smaller version).
//!
//! Like Go, a column with null count 0 gets an all-not-null bitmap of `(length + 7) / 8`
//! bytes before the data is sliced, so a length near 2^31 allocates ~256 MB before the decode
//! fails. The overwritten values here stay below 2^24 to keep the test light; bounding that
//! allocation by the buffer size is a D07.3 question (the response decoder), not a port
//! difference.

use std::panic::AssertUnwindSafe;
use std::panic::catch_unwind;

use bytes::Bytes;
use tidb_base::mysql;
use tidb_chunk::chunk;
use tidb_chunk::codec;
use tidb_parser::types::FieldType;

fn fts() -> Vec<FieldType> {
    vec![
        FieldType::new(mysql::TypeLonglong),
        FieldType::new(mysql::TypeVarchar),
        FieldType::new(mysql::TypeDouble),
    ]
}

/// xorshift64: deterministic, no dependency.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// Decodes `b`, reads every cell, re-encodes. Panics on malformed input.
fn decode_all(b: Vec<u8>) {
    let codec = codec::new_codec(fts());
    let (chk, _rest) = codec.decode(Bytes::from(b));
    for i in 0..chk.num_rows() {
        let r = chk.get_row(i);
        if !r.is_null(0) {
            std::hint::black_box(r.get_int64(0));
        }
        if !r.is_null(1) {
            std::hint::black_box(r.get_bytes(1));
        }
        if !r.is_null(2) {
            std::hint::black_box(r.get_float64(2));
        }
    }
    std::hint::black_box(codec.encode(&chk));
}

#[test]
fn decode_mutated_bytes() {
    let mut c = chunk::new_chunk_with_capacity(&fts(), 300);
    for i in 0..300i64 {
        c.append_int64(0, i * 7);
        if i % 5 == 0 {
            c.append_null(1);
        } else {
            c.append_string(1, format!("value-{i}").as_bytes());
        }
        c.append_float64(2, i as f64 / 3.0);
    }
    let good = codec::new_codec(fts()).encode(&c);
    decode_all(good.clone());

    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let mut rng = Rng(0x9e3779b97f4a7c15);
    let (mut decoded, mut panicked) = (0, 0);
    // Every 4-byte aligned position is tried as a length / null count (the column headers'
    // positions depend on the earlier columns).
    for _ in 0..5_000 {
        let mut b = good.clone();
        match rng.below(3) {
            0 => b.truncate(rng.below(good.len())),
            1 => {
                for _ in 0..1 + rng.below(4) {
                    let i = rng.below(b.len());
                    b[i] ^= 1 << rng.below(8);
                }
            }
            _ => {
                let i = rng.below(b.len() / 4) * 4;
                let v: u32 = [0x00ff_ffff, 0x0001_0000, 0x0000_0301, 0, 1][rng.below(5)];
                b[i..i + 4].copy_from_slice(&v.to_le_bytes());
            }
        }
        match catch_unwind(AssertUnwindSafe(|| decode_all(b))) {
            Ok(()) => decoded += 1,
            Err(_) => panicked += 1,
        }
    }
    std::panic::set_hook(hook);
    // Both outcomes occur (the mutations are not all harmless, nor all fatal).
    assert!(
        decoded > 300 && panicked > 300,
        "{decoded} decoded, {panicked} panicked"
    );
}
