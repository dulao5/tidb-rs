//! `url.Parse` error texts quote the input with Go's `strconv.Quote` (`%q`), not Rust `{:?}`
//! (Lucky's review of tidb-rs #66). Expected values from Go 1.25.12.

use tidb_base::gostd::url;

#[test]
fn parse_error_text_uses_strconv_quote() {
    let cases: &[(&str, &str)] = &[
        (
            "b\n\x1bwg",
            r#"parse "b\n\x1bwg": net/url: invalid control character in URL"#,
        ),
        (
            "x\x00",
            r#"parse "x\x00": net/url: invalid control character in URL"#,
        ),
        ("%ä", r#"parse "%ä": invalid URL escape "%ä""#),
    ];
    for (input, want) in cases {
        let got = url::parse(input).map(|_| ()).unwrap_err();
        assert_eq!(&got, want, "input {input:?}");
    }
}

#[test]
fn strconv_quote_matches_go() {
    use tidb_base::gofmt::strconv_quote;
    assert_eq!(strconv_quote(b"a\"b\\c"), r#""a\"b\\c""#);
    assert_eq!(strconv_quote(b"\x1b\x7f\t"), r#""\x1b\x7f\t""#);
    assert_eq!(strconv_quote("é\u{2028}".as_bytes()), "\"é\\u2028\"");
    assert_eq!(strconv_quote(b"\xc3"), r#""\xc3""#);
}
