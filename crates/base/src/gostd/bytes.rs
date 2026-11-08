//! `bytes.Fields`, `bytes.TrimSpace` and `unicode.IsSpace` (Go 1.25.12), used by
//! `pkg/util/cgroup` (D15.3) to split `/proc` and cgroup files.

use super::decode_rune;

/// `unicode.IsSpace`: Latin-1 `\t \n \v \f \r`, space, U+0085, U+00A0, and the
/// `White_Space` table above Latin-1.
// go-std: unicode.IsSpace L1
pub fn is_space(r: char) -> bool {
    matches!(
        r as u32,
        0x09..=0x0d
            | 0x20
            | 0x85
            | 0xa0
            | 0x1680
            | 0x2000..=0x200a
            | 0x2028
            | 0x2029
            | 0x202f
            | 0x205f
            | 0x3000
    )
}

/// `utf8.DecodeLastRune`: the last rune of `s` and its width; `(RuneError, 1)` for an
/// invalid encoding, `(RuneError, 0)` for empty input.
// go-std: unicode/utf8.DecodeLastRune L2 (via decode_rune on the last 1..=4 bytes)
pub fn decode_last_rune(s: &[u8]) -> (char, usize) {
    let end = s.len();
    if end == 0 {
        return ('\u{fffd}', 0);
    }
    if s[end - 1] < 0x80 {
        return (char::from(s[end - 1]), 1);
    }
    // Go: guard against O(n^2) by looking back at most UTFMax bytes for a start byte.
    let lim = end.saturating_sub(4);
    let mut start = end - 1;
    while start > lim {
        // RuneStart: not a continuation byte.
        if s[start] & 0xc0 != 0x80 {
            break;
        }
        start -= 1;
    }
    let (r, size) = decode_rune(&s[start..end]);
    if start + size != end {
        return ('\u{fffd}', 1);
    }
    (r, size)
}

/// `bytes.Fields`: the slices of `s` between runs of `unicode.IsSpace` runes; an invalid
/// UTF-8 byte is a (non-space) `RuneError`.
// go-std: bytes.Fields L1
// go-std: bytes.FieldsFunc L1 (with unicode.IsSpace)
pub fn fields(s: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut start: Option<usize> = None;
    let mut i = 0;
    while i < s.len() {
        let (r, size) = if s[i] < 0x80 {
            (char::from(s[i]), 1)
        } else {
            decode_rune(&s[i..])
        };
        if is_space(r) {
            if let Some(st) = start.take() {
                out.push(&s[st..i]);
            }
        } else if start.is_none() {
            start = Some(i);
        }
        i += size;
    }
    if let Some(st) = start {
        out.push(&s[st..]);
    }
    out
}

/// `bytes.TrimSpace`: `s` without leading and trailing `unicode.IsSpace` runes.
// go-std: bytes.TrimSpace L1
pub fn trim_space(s: &[u8]) -> &[u8] {
    let mut start = 0;
    while start < s.len() {
        let (r, size) = if s[start] < 0x80 {
            (char::from(s[start]), 1)
        } else {
            decode_rune(&s[start..])
        };
        if !is_space(r) {
            break;
        }
        start += size;
    }
    let mut stop = s.len();
    while stop > start {
        let (r, size) = decode_last_rune(&s[start..stop]);
        if !is_space(r) {
            break;
        }
        stop -= size;
    }
    &s[start..stop]
}
