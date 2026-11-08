//! Unit tests for what the govec vectors do not cover: the allocators (`M-POOL`), the
//! memory accounting of column buffers (A1, N5a 3.7), the copy on write of shared buffers
//! (N5a 3.2, 3.4) and the debug assertion against writes through a column alias.

use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering::SeqCst;

use bytes::Bytes;
use tidb_base::mysql;
use tidb_chunk::Chunk;
use tidb_chunk::ColAlloc;
use tidb_chunk::alloc;
use tidb_chunk::chunk;
use tidb_chunk::codec;
use tidb_memory::TrackedAlloc;
use tidb_memory::tracker::new_tracker;
use tidb_parser::types::FieldType;

fn fts() -> Vec<FieldType> {
    vec![
        FieldType::new(mysql::TypeLonglong),
        FieldType::new(mysql::TypeVarchar),
    ]
}

fn fill(c: &mut Chunk, n: i64) {
    for i in 0..n {
        c.append_int64(0, i);
        if i % 3 == 0 {
            c.append_null(1);
        } else {
            c.append_string(1, format!("v{i}").as_bytes());
        }
    }
}

#[test]
fn allocators_never_reuse() {
    alloc::init_chunk_alloc_size(1, 1);
    for mut a in [
        alloc::new_allocator(),
        alloc::new_empty_allocator(),
        alloc::new_sync_allocator(alloc::new_allocator()),
    ] {
        assert!(!a.check_reuse_alloc_size());
        let c = a.alloc(&fts(), 8, 32);
        let want = chunk::new(&fts(), 8, 32);
        assert_eq!((c.capacity(), c.required_rows(), c.num_cols()), (8, 32, 2));
        assert_eq!(c.memory_usage(), want.memory_usage());
        a.reset();
    }
    let calls = Arc::new(AtomicUsize::new(0));
    let k = calls.clone();
    let mut a = alloc::new_reuse_hook_allocator(
        alloc::new_empty_allocator(),
        Box::new(move || {
            k.fetch_add(1, SeqCst);
        }),
    );
    let _ = a.alloc(&fts(), 4, 4);
    assert!(!a.check_reuse_alloc_size());
    a.reset();
    // The hook runs on the first reused allocation; nothing is ever reused.
    assert_eq!(calls.load(SeqCst), 0);
}

#[test]
fn destroy_data_for_test_changes_the_data() {
    let mut c = chunk::new(&fts(), 4, 4);
    fill(&mut c, 4);
    let before = c.column(1).data_bytes().to_vec();
    c.column_mut(1).destroy_data_for_test();
    assert_ne!(c.column(1).data_bytes(), &before[..]);
    // Destroy drops the chunk (no pool).
    c.destroy(4, &fts());
}

#[test]
fn tracked_columns_account_their_capacity() {
    let root = new_tracker(1, -1);
    let a = TrackedAlloc::new(root.clone());
    {
        let mut c = chunk::new_in(&fts(), 32, 1024, ColAlloc::Tracked(a.clone()));
        fill(&mut c, 5000);
        a.flush();
        let consumed = root.bytes_consumed();
        // Data, null bitmaps and offsets of both columns, at Go's capacities.
        assert!(consumed > 5000 * 8, "{consumed}");
        assert!(consumed <= c.memory_usage());
        let copy = c.copy_construct();
        a.flush();
        // CopyConstruct allocates through the source column's allocator.
        assert!(root.bytes_consumed() > consumed);
        drop(copy);
    }
    a.flush();
    assert_eq!(root.bytes_consumed(), 0);
}

#[test]
fn decoded_columns_share_the_input_and_copy_on_write() {
    let mut c = chunk::new(&fts(), 4, 4);
    fill(&mut c, 10);
    let enc = codec::new_codec(fts()).encode(&c);
    let input = Bytes::from(enc.clone());
    let (mut d, rest) = codec::new_codec(fts()).decode(input.clone());
    assert!(rest.is_empty());
    assert!(d.column(0).is_shared() && d.column(1).is_shared());
    // Writes copy the shared buffer; the input is unchanged.
    d.column_mut(1).set_null(1, true);
    d.column_mut(0).int64s_mut()[0].set(-7);
    assert_eq!(&input[..], &enc[..]);
    assert!(d.get_row(1).is_null(1));
    assert_eq!(d.get_row(0).get_int64(0), -7);
    assert!(!d.column(0).is_shared());
}

#[test]
fn reuse_interm_chk_does_not_write_the_input() {
    let mut c = chunk::new(&fts(), 16, 16);
    fill(&mut c, 16);
    let enc = codec::new_codec(fts()).encode(&c);
    let input = Bytes::from(enc.clone());
    let mut dec = codec::new_decoder(chunk::new_chunk_with_capacity(&fts(), 0), fts());
    dec.reset(input.clone());
    let mut dst = chunk::new(&fts(), 16, 16);
    dst.set_required_rows(8, 16);
    dec.decode(&mut dst);
    let mut dst2 = chunk::new(&fts(), 16, 16);
    // offsets[0] of the remaining rows is not 0: the offsets are adjusted on a copy.
    dec.reuse_interm_chk(&mut dst2);
    assert!(dec.is_finished());
    assert_eq!(&input[..], &enc[..]);
    assert_eq!(dst2.num_rows(), 8);
    for i in 0..8 {
        assert_eq!(dst2.get_row(i).get_int64(0), i as i64 + 8);
        let r = c.get_row(i + 8);
        assert_eq!(dst2.get_row(i).is_null(1), r.is_null(1));
        if !r.is_null(1) {
            assert_eq!(dst2.get_row(i).get_string(1), r.get_string(1));
        }
    }
}

#[test]
#[cfg(debug_assertions)]
#[should_panic(expected = "N5a 3.2")]
fn writing_an_aliased_column_panics_in_debug_builds() {
    let mut c = chunk::new(
        &[
            FieldType::new(mysql::TypeLonglong),
            FieldType::new(mysql::TypeLonglong),
        ],
        4,
        4,
    );
    c.make_ref(0, 1);
    c.append_int64(0, 1);
}

#[test]
fn reset_keeps_slot_aliases() {
    let mut c = chunk::new(
        &[
            FieldType::new(mysql::TypeLonglong),
            FieldType::new(mysql::TypeLonglong),
        ],
        4,
        4,
    );
    c.append_int64(0, 1);
    c.append_int64(1, 2);
    c.make_ref(0, 1);
    c.reset();
    assert!(tidb_chunk::ColArc::ptr_eq(c.column_arc(0), c.column_arc(1)));
    assert_eq!(c.num_rows(), 0);
}

#[test]
fn chunks_are_send_and_sync() {
    fn check<T: Send + Sync>() {}
    check::<Chunk>();
    check::<tidb_chunk::List>();
}
