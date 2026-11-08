//! `gostd::bufio::scan_lines` at the `bufio.MaxScanTokenSize` (64 KiB) boundary.
//!
//! The expected results were produced by Go 1.25.12 with a default `bufio.Scanner`
//! (`ScanLines`) over the same inputs (`len(sc.Bytes())` of each token, then `sc.Err()`):
//!
//! ```text
//! 65535+LF+next:          tokens=[65535 4] err=<nil>
//! 65535 eof:              tokens=[65535]   err=<nil>
//! first+65535+LF:         tokens=[5 65535] err=<nil>
//! 65535 incl CR +LF+next: tokens=[65534 4] err=<nil>
//! 65536+LF+next:          tokens=[]        err=bufio.Scanner: token too long
//! 65536 eof:              tokens=[]        err=bufio.Scanner: token too long
//! first+65536+LF:         tokens=[5]       err=bufio.Scanner: token too long
//! 65536 incl CR +LF+next: tokens=[]        err=bufio.Scanner: token too long
//! 65537 (all four cases): same as 65536
//! ```
//!
//! `scan_lines` returns the tokens yielded before the scanner stopped; a "token too long"
//! stop shows as the missing remaining tokens.

use tidb_base::gostd::bufio::MAX_SCAN_TOKEN_SIZE;
use tidb_base::gostd::bufio::scan_lines;

fn line(n: usize) -> Vec<u8> {
    vec![b'a'; n]
}

fn lens(data: &[u8]) -> Vec<usize> {
    scan_lines(data).iter().map(|t| t.len()).collect()
}

fn cat(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
}

#[test]
fn max_scan_token_size_is_go_value() {
    assert_eq!(MAX_SCAN_TOKEN_SIZE, 65536);
}

#[test]
fn line_of_65535_bytes_is_yielded() {
    let l = line(65535);
    assert_eq!(lens(&cat(&[&l, b"\n", b"next\n"])), [65535, 4]);
    assert_eq!(lens(&l), [65535]);
    assert_eq!(lens(&cat(&[b"first\n", &l, b"\n"])), [5, 65535]);
    // 65534 bytes + CR + LF: the raw line is 65535 bytes, the token drops the CR.
    let cr = cat(&[&line(65534), b"\r\n", b"next\n"]);
    assert_eq!(lens(&cr), [65534, 4]);
}

#[test]
fn line_of_65536_bytes_stops_the_scan() {
    let l = line(65536);
    assert_eq!(lens(&cat(&[&l, b"\n", b"next\n"])), Vec::<usize>::new());
    assert_eq!(lens(&l), Vec::<usize>::new());
    assert_eq!(lens(&cat(&[b"first\n", &l, b"\n"])), [5]);
    // 65535 bytes + CR + LF: the raw line (with the CR) is 65536 bytes.
    let cr = cat(&[&line(65535), b"\r\n", b"next\n"]);
    assert_eq!(lens(&cr), Vec::<usize>::new());
}

#[test]
fn line_of_65537_bytes_stops_the_scan() {
    let l = line(65537);
    assert_eq!(lens(&cat(&[&l, b"\n", b"next\n"])), Vec::<usize>::new());
    assert_eq!(lens(&l), Vec::<usize>::new());
    assert_eq!(lens(&cat(&[b"first\n", &l, b"\n"])), [5]);
    let cr = cat(&[&line(65536), b"\r\n", b"next\n"]);
    assert_eq!(lens(&cr), Vec::<usize>::new());
}
