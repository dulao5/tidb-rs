//! go-file: pkg/types/helper.go
//!
//! Only the leaf functions `mydecimal.go` uses (block D01.1 counts them as done; see
//! specs/D01.2-mydecimal/spec.md §3).

use crate::errors::{ERR_BAD_NUMBER, ERR_TRUNCATED, Error};
use crate::gostd::trim_space;

// go: isSpace L1
pub(crate) fn is_space(c: u8) -> bool {
    c == b' ' || c == b'\t'
}

// go: isDigit L1
pub(crate) fn is_digit(c: u8) -> bool {
    c.is_ascii_digit()
}

const MAX_UINT: u64 = u64::MAX;
const UINT_CUT_OFF: u64 = MAX_UINT / 10 + 1;
const INT_CUT_OFF: u64 = i64::MAX as u64 + 1;

/// strToInt converts a string to an integer in best effort.
/// Go returns `(int64, error)`; the value is meaningful together with the error.
// go: strToInt L1
pub(crate) fn str_to_int(str: &[u8]) -> (i64, Result<(), Error>) {
    let str = trim_space(str);
    if str.is_empty() {
        return (0, Err(ERR_TRUNCATED));
    }
    let mut negative = false;
    let mut i = 0;
    if str[i] == b'-' {
        negative = true;
        i += 1;
    } else if str[i] == b'+' {
        i += 1;
    }

    let mut err: Result<(), Error> = Ok(());
    let mut has_num = false;
    let mut r: u64 = 0;
    while i < str.len() {
        // Go: unicode.IsDigit(rune(str[i])). A single byte is a rune <= MaxLatin1, for which
        // unicode.IsDigit is exactly '0' <= r <= '9'.
        if !str[i].is_ascii_digit() {
            err = Err(ERR_TRUNCATED);
            break;
        }
        has_num = true;
        if r >= UINT_CUT_OFF {
            r = 0;
            err = Err(ERR_BAD_NUMBER);
            break;
        }
        r *= 10;

        // Go uint64 addition wraps; the `r1 < r` test below relies on it.
        let r1 = r.wrapping_add(u64::from(str[i] - b'0'));
        #[allow(clippy::absurd_extreme_comparisons)]
        if r1 < r || r1 > MAX_UINT {
            r = 0;
            err = Err(ERR_BAD_NUMBER);
            break;
        }
        r = r1;
        i += 1;
    }
    if !has_num {
        err = Err(ERR_TRUNCATED);
    }

    if !negative && r >= INT_CUT_OFF {
        return (i64::MAX, Err(ERR_BAD_NUMBER));
    }

    if negative && r > INT_CUT_OFF {
        return (i64::MIN, Err(ERR_BAD_NUMBER));
    }

    if negative {
        r = r.wrapping_neg();
    }
    (r as i64, err)
}
