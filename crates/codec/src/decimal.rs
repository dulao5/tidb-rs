//! go-file: pkg/util/codec/decimal.go
//!
//! Decimal key/value encoding: precision and frac bytes followed by `MyDecimal.WriteBin`.
//! The `errorInDecodeDecimal` failpoint of `DecodeDecimal` is not ported (no failpoints in
//! tidb-rs yet).

use tidb_base::Error;
use tidb_base::errors;
use tidb_base::mysql;
use tidb_types::MyDecimal;
use tidb_types::decimal_bin_size;

/// EncodeDecimal encodes a decimal into a byte slice which can be sorted lexicographically later.
///
/// Go returns `b` (as `WriteBin` left it) together with the error.
// go: EncodeDecimal L1 [M-ERRVAL]
pub fn encode_decimal(
    b: &mut Vec<u8>,
    dec: &MyDecimal,
    mut precision: isize,
    mut frac: isize,
) -> Result<(), Error> {
    if precision == 0 {
        (precision, frac) = dec.precision_and_frac();
    }
    if frac > mysql::MaxDecimalScale {
        frac = mysql::MaxDecimalScale;
    }
    b.push(precision as u8);
    b.push(frac as u8);
    let buf = std::mem::take(b);
    let (buf, err) = dec.write_bin(precision, frac, buf);
    *b = buf;
    err
}

// go: valueSizeOfDecimal L1
pub(crate) fn value_size_of_decimal(
    dec: &MyDecimal,
    mut precision: isize,
    mut frac: isize,
) -> Result<isize, Error> {
    if precision == 0 {
        (precision, frac) = dec.precision_and_frac();
    }
    let bin_size = decimal_bin_size(precision, frac)?;
    Ok(bin_size + 2)
}

/// DecodeDecimal decodes bytes to decimal.
///
/// Returns `(remain, dec, precision, frac, err)` as Go does: on a `FromBin` error the remain
/// is already advanced by the bin size `FromBin` reported and `dec` is `None` (Go `nil`).
// go: DecodeDecimal L1 [M-ERRVAL]
pub fn decode_decimal(b: &[u8]) -> (&[u8], Option<MyDecimal>, isize, isize, Result<(), Error>) {
    if b.len() < 3 {
        return (
            b,
            None,
            0,
            0,
            Err(errors::new("insufficient bytes to decode value")),
        );
    }
    let precision = b[0] as isize;
    let frac = b[1] as isize;
    let b = &b[2..];
    let mut dec = MyDecimal::default();
    let (bin_size, err) = dec.from_bin(b, precision, frac);
    let b = &b[bin_size as usize..];
    if let Err(err) = err {
        return (b, None, precision, frac, Err(err));
    }
    (b, Some(dec), precision, frac, Ok(()))
}
