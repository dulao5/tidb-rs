//! go-file: pkg/types/fsp.go

#![allow(non_upper_case_globals)]

use tidb_base::{Error, errors};

use crate::gostd::parse_int;

/// UnspecifiedFsp is the unspecified fractional seconds part.
pub const UnspecifiedFsp: isize = -1;
/// MaxFsp is the maximum digit of fractional seconds part.
pub const MaxFsp: isize = 6;
/// MinFsp is the minimum digit of fractional seconds part.
pub const MinFsp: isize = 0;
/// DefaultFsp is the default digit of fractional seconds part.
/// MySQL use 0 as the default Fsp.
pub const DefaultFsp: isize = 0;

/// CheckFsp checks whether fsp is in valid range.
// go: CheckFsp L1 [M-ERRVAL]
pub fn check_fsp(fsp: isize) -> (isize, Result<(), Error>) {
    if fsp == UnspecifiedFsp {
        return (DefaultFsp, Ok(()));
    }
    if fsp < MinFsp {
        return (
            DefaultFsp,
            Err(errors::errorf(
                "Invalid fsp %d",
                &[tidb_base::gofmt::Arg::from(fsp)],
            )),
        );
    } else if fsp > MaxFsp {
        return (MaxFsp, Ok(()));
    }
    (fsp, Ok(()))
}

/// `math.Pow10(n)` for the small non-negative `n` used here (exact in float64).
// go-std: math.Pow10 L2 (0 <= n <= 9 only)
pub(crate) fn pow10(n: isize) -> f64 {
    10f64.powi(n as i32)
}

/// ParseFrac parses the input string according to fsp, returns the microsecond,
/// and also a bool value to indice overflow. eg:
/// "999" fsp=2 will overflow.
// go: ParseFrac L1 [M-ERRVAL, M-F2I]
pub fn parse_frac(s: &[u8], fsp: isize) -> Result<(isize, bool), Error> {
    if s.is_empty() {
        return Ok((0, false));
    }

    let (fsp, err) = check_fsp(fsp);
    err?;

    if fsp as usize >= s.len() {
        let (tmp, e) = parse_int(s, 64);
        e.map_err(|e| e.into_base())?;
        // [M-F2I] float64(tmp) * math.Pow10(MaxFsp-len(s)) is a small exact value
        let v = tidb_base::gostd::conv::f64_to_i64(tmp as f64 * pow10(MaxFsp - s.len() as isize));
        return Ok((v as isize, false));
    }

    // Round when fsp < string length.
    let (tmp, e) = parse_int(&s[..fsp as usize + 1], 64);
    e.map_err(|e| e.into_base())?;
    let tmp = (tmp + 5) / 10;

    if tmp as f64 >= pow10(fsp) {
        // overflow
        return Ok((0, true));
    }

    // Get the final frac, with 6 digit number
    //  1236 round 3 -> 124 -> 124000
    //  0312 round 2 -> 3 -> 30000
    //  999 round 2 -> 100 -> overflow
    // [M-F2I]
    let v = tidb_base::gostd::conv::f64_to_i64(tmp as f64 * pow10(MaxFsp - fsp));
    Ok((v as isize, false))
}

/// alignFrac is used to generate alignment frac, like `100` -> `100000` ,`-100` -> `-100000`
// go: alignFrac L1
pub(crate) fn align_frac(s: &[u8], fsp: isize) -> Vec<u8> {
    let mut sl = s.len() as isize;
    if sl > 0 && s[0] == b'-' {
        sl -= 1;
    }
    let mut out = s.to_vec();
    if sl < fsp {
        out.extend(std::iter::repeat_n(b'0', (fsp - sl) as usize));
    }
    out
}
