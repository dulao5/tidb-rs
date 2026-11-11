//! `DeRedact` with a line over 64 KiB (tidb-rs #89 review, upstream pitfall #90).
//!
//! Go's `bufio.Scanner` stops silently at such a line and DeRedact returns nil, so
//! `tidb-server --redact` drops that line and every line after it. tidb-rs keeps all
//! lines (a deliberate, user-visible deviation; see docs/compatibility.md).

use tidb_base::redact::de_redact;

#[test]
fn de_redact_keeps_lines_after_a_long_line() {
    let long = format!("‹{}›", "x".repeat(70 * 1024));
    let input = format!("first ‹a›\n{long}\nlast ‹b›\n");
    let mut out = Vec::new();
    de_redact(false, input.as_bytes(), &mut out, "\n").unwrap();
    let out = String::from_utf8(out).unwrap();
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 3, "all three lines are kept");
    assert_eq!(lines[0], "first a");
    assert_eq!(lines[1].len(), 70 * 1024);
    assert_eq!(lines[2], "last b");

    let mut out = Vec::new();
    de_redact(true, input.as_bytes(), &mut out, "\n").unwrap();
    let out = String::from_utf8(out).unwrap();
    assert_eq!(out.lines().collect::<Vec<_>>(), ["first ?", "?", "last ?"]);
}
