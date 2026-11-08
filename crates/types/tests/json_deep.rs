//! Deeply nested JSON texts (Lucky's review of tidb-rs #21): any nesting up to
//! `json.Valid`'s limit is handled without recursion and without spawning threads, so
//! parsing works on small stacks and under concurrency. Results are checked against Go by
//! the govec `sys/deep` records; this test checks the resource behaviour.

use tidb_types::json::{self as j, BinaryJSON, ErrJSONDocumentTooDeep};

fn nested(d: usize, inner: &str) -> Vec<u8> {
    format!("{}{inner}{}", "[".repeat(d), "]".repeat(d)).into_bytes()
}

/// 16 threads with 256 KiB stacks parse texts nested 300 and 10000 levels deep at once.
/// A recursive parser would overflow these stacks; the old large-stack thread per document
/// failed to spawn under address-space limits.
#[test]
fn deep_texts_on_small_stacks_concurrently() {
    let handles: Vec<_> = (0..16)
        .map(|i| {
            std::thread::Builder::new()
                .name(format!("json-deep-{i}"))
                .stack_size(256 << 10)
                .spawn(move || {
                    let depth = if i % 2 == 0 { 300 } else { 10000 };
                    for _ in 0..20 {
                        let text = nested(depth, "1");
                        let err = j::parse_binary_json_from_string(&text).unwrap_err();
                        assert!(ErrJSONDocumentTooDeep.equal(&err), "{err}");
                        assert!(j::json_valid(&text));
                        let err = BinaryJSON::default().unmarshal_json(&text).unwrap_err();
                        assert!(ErrJSONDocumentTooDeep.equal(&err), "{err}");
                        // Nested one level more than json.Valid allows.
                        assert!(!j::json_valid(&nested(10001, "")));
                    }
                })
                .expect("spawn")
        })
        .collect();
    for h in handles {
        h.join().expect("no panic");
    }
}

/// Every nesting that converts (up to maxJSONDepth + 1 with an empty innermost container)
/// goes through the recursive path, whose depth is bounded by that nesting: 1 MiB is enough
/// even for unoptimized builds (tokio and std threads default to 2 MiB).
#[test]
fn deepest_convertible_texts_on_a_bounded_stack() {
    std::thread::Builder::new()
        .name("json-convertible".to_owned())
        .stack_size(1 << 20)
        .spawn(|| {
            let bj = j::parse_binary_json_from_string(&nested(101, "")).unwrap();
            assert_eq!(bj.get_elem_depth(), 101);
            let bj = j::parse_binary_json_from_string(&nested(100, "1")).unwrap();
            assert_eq!(bj.get_elem_depth(), 101);
            let err = j::parse_binary_json_from_string(&nested(101, "1")).unwrap_err();
            assert!(ErrJSONDocumentTooDeep.equal(&err), "{err}");
        })
        .expect("spawn")
        .join()
        .expect("no panic");
}
