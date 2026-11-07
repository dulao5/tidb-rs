//! go-file: pkg/parser/mysql/locale_format.go

use crate::errors::{self, Error};
use crate::gostd::{is_digit, is_digit_byte};

/// Go `strconv.ParseUint(s, 10, 64)` (success only; the error value is not used here).
// go-std: strconv.ParseUint L2 (base 10, bitSize 64; returns None on any error)
fn parse_uint(s: &[u8]) -> Option<u64> {
    if s.is_empty() {
        return None;
    }
    let mut n: u64 = 0;
    for &c in s {
        if !c.is_ascii_digit() {
            return None;
        }
        n = n.checked_mul(10)?.checked_add(u64::from(c - b'0'))?;
    }
    Some(n)
}

/// `formatENUS`. Works on the bytes of the Go strings (a non-ASCII digit in `number` is cut
/// at byte offsets as in Go); the result is converted to a Rust string lossily.
// go: formatENUS L1 [M-INT]
#[allow(clippy::if_same_then_else)] // Go's three `continue` branches
pub fn formatENUS(number: &str, precision: &str) -> Result<String, Error> {
    let mut buffer: Vec<u8> = Vec::new();
    let mut precision: &[u8] = precision.as_bytes();
    let number_owned: Vec<u8>;
    let mut number: &[u8] = number.as_bytes();
    if is_digit_byte(precision[0]) {
        let ps = std::str::from_utf8(precision).unwrap_or("");
        for (i, v) in ps.char_indices() {
            if is_digit(v) {
                continue;
            }
            precision = &precision[..i];
            break;
        }
    } else {
        precision = b"0";
    }
    if number[0] == b'-' && number[1] == b'.' {
        number_owned = replace1(number, b"-", b"-0");
        number = &number_owned;
    } else if number[0] == b'.' {
        number_owned = replace1(number, b".", b"0.");
        number = &number_owned;
    }

    if (&number[..1] == b"-" && !is_digit_byte(number[1]))
        || (!is_digit_byte(number[0]) && &number[..1] != b"-")
    {
        buffer.push(b'0');
        let position = parse_uint(precision);
        if let Some(position) = position
            && position > 0
        {
            buffer.push(b'.');
            buffer.extend(std::iter::repeat_n(b'0', position as usize));
        }
        return Ok(String::from_utf8_lossy(&buffer).into_owned());
    } else if &number[..1] == b"-" {
        buffer.push(b'-');
        number = &number[1..];
    }

    let ns = String::from_utf8_lossy(number).into_owned();
    for (i, v) in ns.char_indices() {
        if is_digit(v) {
            continue;
        } else if i == 1 && number[1] == b'.' {
            continue;
        } else if v == '.' && number[1] != b'.' {
            continue;
        }
        number = &number[..i];
        break;
    }

    let comma = b',';
    let parts: Vec<&[u8]> = number.split(|&c| c == b'.').collect();
    let mut pos = 0usize;
    if !parts[0].len().is_multiple_of(3) {
        pos += parts[0].len() % 3;
        buffer.extend_from_slice(&parts[0][..pos]);
        buffer.push(comma);
    }
    while pos < parts[0].len() {
        buffer.extend_from_slice(&parts[0][pos..pos + 3]);
        buffer.push(comma);
        pos += 3;
    }
    buffer.truncate(buffer.len() - 1);

    if let Some(position) = parse_uint(precision)
        && position > 0
    {
        buffer.push(b'.');
        if parts.len() == 2 {
            if parts[1].len() as u64 >= position {
                buffer.extend_from_slice(&parts[1][..position as usize]);
            } else {
                buffer.extend_from_slice(parts[1]);
                buffer.extend(std::iter::repeat_n(
                    b'0',
                    (position as isize - parts[1].len() as isize) as usize,
                ));
            }
        } else {
            buffer.extend(std::iter::repeat_n(b'0', position as usize));
        }
    }

    Ok(String::from_utf8_lossy(&buffer).into_owned())
}

/// `strings.Replace(s, old, new, 1)`.
// go-std: strings.Replace L2 (n = 1, non-empty old)
fn replace1(s: &[u8], old: &[u8], new: &[u8]) -> Vec<u8> {
    match s.windows(old.len()).position(|w| w == old) {
        Some(i) => [&s[..i], new, &s[i + old.len()..]].concat(),
        None => s.to_vec(),
    }
}

// go: formatZHCN L1
pub fn formatZHCN(_: &str, _: &str) -> Result<String, Error> {
    Err(errors::new("not implemented"))
}

// go: formatNotSupport L1
pub fn formatNotSupport(_: &str, _: &str) -> Result<String, Error> {
    Err(errors::new("not support for the specific locale"))
}
