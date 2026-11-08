//! `gostd::path::rel_bytes` against Go's `filepath.Rel` test table (`reltests` of
//! $GOROOT/src/path/filepath/path_test.go, Go 1.25.12, Unix), and spot checks of
//! `gostd::bytes`, `gostd::bufio` and `gostd::strconv::parse_uint10` (their full coverage
//! is the govec `cgroup` domain of tidb-memory). The `..`-base error of `Rel` cannot be
//! reached from pkg/util/cgroup (it skips mount roots that contain `..`), so this table is
//! its check.

use tidb_base::gostd::bufio::scan_lines;
use tidb_base::gostd::bytes::fields;
use tidb_base::gostd::bytes::trim_space;
use tidb_base::gostd::path::rel_bytes;
use tidb_base::gostd::strconv::parse_uint10;

#[test]
fn rel_go_reltests() {
    let reltests: &[(&str, &str, &str)] = &[
        ("a/b", "a/b", "."),
        ("a/b/.", "a/b", "."),
        ("a/b", "a/b/.", "."),
        ("./a/b", "a/b", "."),
        ("a/b", "./a/b", "."),
        ("ab/cd", "ab/cde", "../cde"),
        ("ab/cd", "ab/c", "../c"),
        ("a/b", "a/b/c/d", "c/d"),
        ("a/b", "a/b/../c", "../c"),
        ("a/b/../c", "a/b", "../b"),
        ("a/b/c", "a/c/d", "../../c/d"),
        ("a/b", "c/d", "../../c/d"),
        ("a/b/c/d", "a/b", "../.."),
        ("a/b/c/d", "a/b/", "../.."),
        ("a/b/c/d/", "a/b", "../.."),
        ("a/b/c/d/", "a/b/", "../.."),
        ("../../a/b", "../../a/b/c/d", "c/d"),
        ("/a/b", "/a/b", "."),
        ("/a/b/.", "/a/b", "."),
        ("/a/b", "/a/b/.", "."),
        ("/ab/cd", "/ab/cde", "../cde"),
        ("/ab/cd", "/ab/c", "../c"),
        ("/a/b", "/a/b/c/d", "c/d"),
        ("/a/b", "/a/b/../c", "../c"),
        ("/a/b/../c", "/a/b", "../b"),
        ("/a/b/c", "/a/c/d", "../../c/d"),
        ("/a/b", "/c/d", "../../c/d"),
        ("/a/b/c/d", "/a/b", "../.."),
        ("/a/b/c/d", "/a/b/", "../.."),
        ("/a/b/c/d/", "/a/b", "../.."),
        ("/a/b/c/d/", "/a/b/", "../.."),
        ("/../../a/b", "/../../a/b/c/d", "c/d"),
        (".", "a/b", "a/b"),
        (".", "..", ".."),
        // can't do purely lexically
        ("..", ".", "err"),
        ("..", "a", "err"),
        ("../..", "..", "err"),
        ("a", "/a", "err"),
        ("/a", "a", "err"),
    ];
    for &(root, path, want) in reltests {
        let got = rel_bytes(root.as_bytes(), path.as_bytes());
        match got {
            Ok(r) => assert_eq!(
                String::from_utf8(r).unwrap(),
                want,
                "Rel({root:?}, {path:?})"
            ),
            Err(e) => {
                assert_eq!(want, "err", "Rel({root:?}, {path:?})");
                assert_eq!(
                    String::from_utf8(e).unwrap(),
                    format!("Rel: can't make {path} relative to {root}")
                );
            }
        }
    }
}

#[test]
fn fields_trim_scan_parse() {
    let mut input = " a\tb\x0b\x0cc\u{a0}d\u{85}e\u{2003}f".as_bytes().to_vec();
    input.extend_from_slice(b"\xffg ");
    let want: Vec<&[u8]> = vec![b"a", b"b", b"c", b"d", b"e", b"f\xffg"];
    assert_eq!(fields(&input), want);
    assert_eq!(trim_space("\u{3000} x y \u{85}\r\n".as_bytes()), b"x y");
    assert_eq!(trim_space(b"\xff "), b"\xff");
    assert_eq!(
        scan_lines(b"a\r\nb\n\nc"),
        vec![b"a".as_slice(), b"b", b"", b"c"]
    );
    assert_eq!(scan_lines(b"a\r\r\n"), vec![b"a\r".as_slice()]);
    assert!(scan_lines(&vec![b'x'; 65536]).is_empty());
    assert_eq!(scan_lines(&vec![b'x'; 65535]).len(), 1);
    assert_eq!(parse_uint10(b"18446744073709551615"), Ok(u64::MAX));
    assert_eq!(
        parse_uint10(b"18446744073709551616").unwrap_err(),
        b"strconv.ParseUint: parsing \"18446744073709551616\": value out of range"
    );
    assert_eq!(
        parse_uint10(b"99999999999999999999x").unwrap_err(),
        b"strconv.ParseUint: parsing \"99999999999999999999x\": value out of range"
    );
    assert_eq!(
        parse_uint10(b"\xff1").unwrap_err(),
        b"strconv.ParseUint: parsing \"\\xff1\": invalid syntax"
    );
}
