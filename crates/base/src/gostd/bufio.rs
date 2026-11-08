//! `bufio.Scanner` with `bufio.ScanLines` (Go 1.25.12), as `pkg/util/cgroup` (D15.3) uses
//! it on whole files.

/// `bufio.MaxScanTokenSize`.
pub const MAX_SCAN_TOKEN_SIZE: usize = 64 * 1024;

/// The tokens a default `bufio.Scanner` with `ScanLines` yields for `data` read to the end:
/// lines split at `\n` with one trailing `\r` dropped, the last line also without a
/// newline when it is not empty. Scanning stops (`ErrTooLong`, the line not yielded) at the
/// first line whose raw length (before dropping `\r`) is `MaxScanTokenSize` or more: the
/// scanner's buffer cannot grow past that size.
// go-std: bufio.Scanner.Scan L2 (whole input in memory; the buffer limit as a length check)
// go-std: bufio.ScanLines L1
// go-std: bufio.dropCR L1
pub fn scan_lines(data: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut rest = data;
    while !rest.is_empty() {
        let (raw, next) = match rest.iter().position(|&b| b == b'\n') {
            Some(i) => (&rest[..i], &rest[i + 1..]),
            None => (rest, &rest[rest.len()..]),
        };
        if raw.len() >= MAX_SCAN_TOKEN_SIZE {
            break;
        }
        out.push(raw.strip_suffix(b"\r").unwrap_or(raw));
        rest = next;
    }
    out
}
