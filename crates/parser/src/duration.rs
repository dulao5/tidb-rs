//! go-file: pkg/parser/duration/duration.go
//!
//! A customized duration, which supports unit 'd', 'h' and 'm' (TTL job intervals).

use tidb_base::gofmt::Arg;
use tidb_base::gostd::conv::f64_to_i64;
use tidb_base::gostd::time::{Duration, HOUR, MINUTE};
use tidb_base::{Error, errors};

/// `readFloat`: the leading run of `unicode.IsDigit` runes and '.', parsed as a float, and
/// the rest of `s`. A string that is all digits (no unit) is an error, as in Go.
// go: readFloat L1 [M-ERR]
fn read_float(s: &str) -> Result<(f64, &str), Error> {
    let mut numbers = "";
    for (pos, ch) in s.char_indices() {
        if !tidb_base::gostd::is_digit(ch) && ch != '.' {
            numbers = &s[..pos];
            break;
        }
    }
    if !numbers.is_empty() {
        return match tidb_base::gostd::strconv::parse_float(numbers) {
            Ok(i) => Ok((i, &s[numbers.len()..])),
            Err(range) => Err(Error::plain(format!(
                "strconv.ParseFloat: parsing {}: {}",
                go_quote(numbers),
                if range {
                    "value out of range"
                } else {
                    "invalid syntax"
                }
            ))),
        };
    }
    Err(errors::new("fail to read an integer"))
}

/// `strconv.Quote` as `*strconv.NumError` prints its input.
// glue: strconv.Quote of the NumError input (via Go fmt %q)
fn go_quote(s: &str) -> String {
    String::from_utf8_lossy(&tidb_base::gofmt::sprintf("%q", &[Arg::from(s.to_owned())]))
        .into_owned()
}

/// ParseDuration parses the duration which contains 'd', 'h' and 'm'.
// go: ParseDuration L1 [M-F2I, M-ERR]
pub fn parse_duration(s: &str) -> Result<Duration, Error> {
    let mut duration: Duration = 0;

    if s == "0" {
        return Ok(0);
    }

    let mut s = s;
    while !s.is_empty() {
        let (i, rest) = read_float(s)?;
        s = rest;
        let unit = s.as_bytes()[0];
        match unit {
            b'd' => duration = duration.wrapping_add(f64_to_i64(i * (HOUR * 24) as f64)),
            b'h' => duration = duration.wrapping_add(f64_to_i64(i * HOUR as f64)),
            b'm' => duration = duration.wrapping_add(f64_to_i64(i * MINUTE as f64)),
            _ => {
                // %c of a byte prints the rune with that value (Latin-1).
                return Err(errors::errorf("unknown unit %c", &[Arg::from(unit)]));
            }
        }
        s = &s[1..];
    }

    Ok(duration)
}
