//! `gostd::json` on documents nested to Go's limit (`maxNestingDepth` = 10000) on a 2 MiB
//! thread stack (tokio's worker default): parsing, conversion to `Value`, encoding and the
//! drop of `Raw` / `Value` trees must not recurse once per level (review of tidb-rs #25).
//! Run in both debug and release: the stack use of a recursive version depends on the
//! optimizer.

use tidb_base::gostd::json;

const MAX: usize = 10000;

/// Runs `f` on a thread with a 2 MiB stack and propagates its panic.
fn on_small_stack<F: FnOnce() + Send + 'static>(f: F) {
    std::thread::Builder::new()
        .stack_size(2 << 20)
        .spawn(f)
        .expect("spawn")
        .join()
        .unwrap_or_else(|p| std::panic::resume_unwind(p));
}

/// `n` nested arrays around `inner`.
fn arrays(n: usize, inner: &str) -> String {
    format!("{}{inner}{}", "[".repeat(n), "]".repeat(n))
}

/// `n` nested objects `{"a":…}` around `inner`.
fn objects(n: usize, inner: &str) -> String {
    format!("{}{inner}{}", r#"{"a":"#.repeat(n), "}".repeat(n))
}

#[test]
fn unmarshal_raw_at_max_depth() {
    on_small_stack(|| {
        for doc in [
            arrays(MAX, ""),
            arrays(MAX, "1"),
            objects(MAX, "1"),
            objects(MAX - 1, "[]"),
        ] {
            let raw = json::unmarshal_raw(doc.as_bytes()).expect("valid at depth 10000");
            drop(raw);
        }
        assert_eq!(
            json::unmarshal_raw(arrays(MAX + 1, "").as_bytes()).unwrap_err(),
            "invalid character '[' exceeded max depth"
        );
        // Unclosed: the open containers of the partial tree are dropped on the error path.
        assert_eq!(
            json::unmarshal_raw("[".repeat(MAX).as_bytes()).unwrap_err(),
            "unexpected end of JSON input"
        );
        assert!(json::unmarshal_raw(objects(MAX, "x").as_bytes()).is_err());
    });
}

#[test]
fn unmarshal_and_encode_at_max_depth() {
    on_small_stack(|| {
        for doc in [
            arrays(MAX, ""),
            arrays(MAX, "1"),
            objects(MAX, "1"),
            objects(MAX - 1, "[]"),
        ] {
            let v = json::unmarshal(doc.as_bytes()).expect("valid at depth 10000");
            assert_eq!(json::marshal(&v), doc);
            let mut out = String::new();
            json::encode_value(&mut out, &v).unwrap();
            assert_eq!(out, doc);
            // Indent with an empty unit: only the newlines grow with the depth.
            let ind = json::marshal_indent(&v, "");
            assert_eq!(ind.replace('\n', "").replace(": ", ":"), doc);
            drop(v);
        }
    });
}

#[test]
fn unmarshal_errors_at_max_depth() {
    on_small_stack(|| {
        // A type error at the bottom (Go saves it and still decodes the rest).
        assert_eq!(
            json::unmarshal(arrays(MAX, "1e400").as_bytes()).unwrap_err(),
            "json: cannot unmarshal number 1e400 into Go value of type float64"
        );
        // A syntax error wins over an earlier type error (checkValid runs first).
        assert_eq!(
            json::unmarshal(format!("[1e400,{}", arrays(MAX - 1, "")).as_bytes()).unwrap_err(),
            "unexpected end of JSON input"
        );
        assert_eq!(
            json::unmarshal(arrays(MAX + 1, "").as_bytes()).unwrap_err(),
            "invalid character '[' exceeded max depth"
        );
    });
}

#[test]
fn duplicate_keys_and_errors_keep_go_order() {
    // Not deep: the explicit-stack conversion keeps Go's map assignment (last key wins) and
    // saves the first type error in document order.
    let v = json::unmarshal(br#"{"a":[1,{"b":2}],"a":{"c":[true,null,"x"]}}"#).unwrap();
    assert_eq!(json::marshal(&v), r#"{"a":{"c":[true,null,"x"]}}"#);
    assert_eq!(
        json::unmarshal(br#"[{"x":1e999},1e400]"#).unwrap_err(),
        "json: cannot unmarshal number 1e999 into Go value of type float64"
    );
}
