//! go-file: pkg/statistics/scalar.go
//!
//! D11.1 part 1 (VS1-29): the scalar fractions behind `Histogram.LessRowCount`.
//! `EnumRangeValues` is D11.1 part 2 (its only caller, `GetColumnRowCount`, is a boundary
//! stub): `// glue: stub of EnumRangeValues, blocked-by:D11.1 (part 2: GetColumnRowCount, stats ver 1 small ranges)`.

use std::sync::LazyLock;

use tidb_base::gostd::time::utc;
use tidb_base::mysql::{TypeDate, TypeDatetime, TypeTimestamp};
use tidb_types::context::{Context, DefaultStmtFlags, IgnoreWarn, new_context};
use tidb_types::datum::*;
use tidb_types::time::{MinDatetime, MinTimestamp, Time, new_time};

use crate::histogram::Histogram;

/// Go's unexported `scalar`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Scalar {
    pub(crate) lower: f64,
    pub(crate) upper: f64,
    /// commonPfxLen is the common prefix length of the lower bound and upper bound when the value type is KindString or KindBytes.
    pub(crate) common_pfx_len: usize,
}

/// EmptyScalarSize is the size of empty scalar (Go's `unsafe.Sizeof(scalar{})`, 64-bit).
pub const EmptyScalarSize: i64 = 24;

/// calcFraction is used to calculate the fraction of the interval [lower, upper] that lies within the [lower, value]
/// using the continuous-value assumption.
// go: calcFraction L1
pub fn calc_fraction(lower: f64, upper: f64, value: f64) -> f64 {
    if upper <= lower {
        return 0.5;
    }
    if value <= lower {
        return 0.0;
    }
    if value >= upper {
        return 1.0;
    }
    let frac = (value - lower) / (upper - lower);
    if frac.is_nan() || frac.is_infinite() || frac < 0.0 || frac > 1.0 {
        return 0.5;
    }
    frac
}

/// UTCWithAllowInvalidDateCtx is introduced for the following reason:
///
///  Invalid date values may be inserted into table under some relaxed sql mode. Those values may exist in statistics.
///  Hence, when reading statistics, we should skip invalid date check. See #39336.
///
/// `types.DefaultStmtFlags|types.FlagIgnoreInvalidDateErr|types.FlagIgnoreZeroInDateErr`.
pub static UTCWithAllowInvalidDateCtx: LazyLock<Context> = LazyLock::new(|| {
    new_context(
        DefaultStmtFlags
            .with_ignore_invalid_date_err(true)
            .with_ignore_zero_in_date(true),
        utc(),
        std::sync::Arc::new(IgnoreWarn),
    )
});

/// convertDatumToScalar.
// go: convertDatumToScalar L1
pub fn convert_datum_to_scalar(value: &Datum, common_pfx_len: usize) -> f64 {
    match value.kind() {
        KindFloat32 => value.get_float32() as f64,
        KindFloat64 => value.get_float64(),
        KindInt64 => value.get_int64() as f64,
        KindUint64 => value.get_uint64() as f64,
        KindMysqlDuration => value.get_mysql_duration().duration as f64,
        KindMysqlDecimal => {
            let (scalar, err) = value.get_mysql_decimal().to_float64();
            if err.is_err() {
                return 0.0;
            }
            scalar
        }
        KindMysqlTime => {
            let value_time = value.get_mysql_time();
            let min_time: Time = match value_time.tp() {
                TypeDate => new_time(MinDatetime, TypeDate, tidb_types::fsp::DefaultFsp),
                TypeDatetime => new_time(MinDatetime, TypeDatetime, tidb_types::fsp::DefaultFsp),
                TypeTimestamp => *MinTimestamp,
                _ => Time::default(),
            };
            value_time
                .sub(&UTCWithAllowInvalidDateCtx, &min_time)
                .duration as f64
        }
        KindString | KindBytes => {
            let bytes = value.get_bytes();
            if bytes.len() <= common_pfx_len {
                return 0.0;
            }
            convert_bytes_to_scalar(&bytes[common_pfx_len..])
        }
        KindMinNotNull => -f64::MAX,
        KindMaxValue => f64::MAX,
        // do not know how to convert
        _ => 0.0,
    }
}

impl Histogram {
    /// PreCalculateScalar converts the lower and upper to scalar. When the datum type is KindString or KindBytes, we also
    /// calculate their common prefix length, because when a value falls between lower and upper, the common prefix
    /// of lower and upper equals to the common prefix of the lower, upper and the value. For some simple types like `Int64`,
    /// we do not convert it because we can directly infer the scalar value.
    // go: Histogram.PreCalculateScalar L1
    pub fn pre_calculate_scalar(&mut self) {
        let l = self.len();
        if l == 0 {
            return;
        }
        match self.get_lower(0).kind() {
            KindMysqlDecimal | KindMysqlTime => {
                let (mut lower, mut upper) = (Datum::default(), Datum::default());
                self.scalars = vec![Scalar::default(); l];
                for i in 0..l {
                    // It's read-only, so we don't need to allocate new datum each time.
                    self.lower_to_datum(i, &mut lower);
                    self.upper_to_datum(i, &mut upper);
                    self.scalars[i].lower = convert_datum_to_scalar(&lower, 0);
                    self.scalars[i].upper = convert_datum_to_scalar(&upper, 0);
                }
            }
            KindBytes | KindString => {
                let (mut lower, mut upper) = (Datum::default(), Datum::default());
                self.scalars = vec![Scalar::default(); l];
                for i in 0..l {
                    // It's read-only, so we don't need to allocate new datum each time.
                    self.lower_to_datum(i, &mut lower);
                    self.upper_to_datum(i, &mut upper);
                    let common = common_prefix_length(&[lower.get_bytes(), upper.get_bytes()]);
                    self.scalars[i].common_pfx_len = common;
                    self.scalars[i].lower = convert_datum_to_scalar(&lower, common);
                    self.scalars[i].upper = convert_datum_to_scalar(&upper, common);
                }
            }
            _ => {}
        }
    }

    /// calcFraction.
    // go: Histogram.calcFraction L1
    pub(crate) fn calc_fraction(&self, index: usize, value: &Datum) -> f64 {
        let (lower, upper) = (
            self.bounds.get_row(2 * index),
            self.bounds.get_row(2 * index + 1),
        );
        match value.kind() {
            KindFloat32 => calc_fraction(
                lower.get_float32(0) as f64,
                upper.get_float32(0) as f64,
                value.get_float32() as f64,
            ),
            KindFloat64 => calc_fraction(
                lower.get_float64(0),
                upper.get_float64(0),
                value.get_float64(),
            ),
            KindInt64 => calc_fraction(
                lower.get_int64(0) as f64,
                upper.get_int64(0) as f64,
                value.get_int64() as f64,
            ),
            KindUint64 => calc_fraction(
                lower.get_uint64(0) as f64,
                upper.get_uint64(0) as f64,
                value.get_uint64() as f64,
            ),
            KindMysqlDuration => calc_fraction(
                lower.get_duration(0, 0).duration as f64,
                upper.get_duration(0, 0).duration as f64,
                value.get_mysql_duration().duration as f64,
            ),
            KindMysqlDecimal | KindMysqlTime => calc_fraction(
                self.scalars[index].lower,
                self.scalars[index].upper,
                convert_datum_to_scalar(value, 0),
            ),
            KindBytes | KindString => calc_fraction(
                self.scalars[index].lower,
                self.scalars[index].upper,
                convert_datum_to_scalar(value, self.scalars[index].common_pfx_len),
            ),
            _ => 0.5,
        }
    }
}

/// commonPrefixLength.
// go: commonPrefixLength L1
pub fn common_prefix_length(strs: &[&[u8]]) -> usize {
    if strs.is_empty() {
        return 0;
    }
    let mut min_len = strs[0].len();
    for s in strs {
        if s.len() < min_len {
            min_len = s.len();
        }
    }
    for i in 0..min_len {
        let a = strs[0][i];
        for s in strs {
            if s[i] != a {
                return i;
            }
        }
    }
    min_len
}

/// convertBytesToScalar.
// go: convertBytesToScalar L1
pub fn convert_bytes_to_scalar(value: &[u8]) -> f64 {
    // Bytes type is viewed as a base-256 value, so we only consider at most 8 bytes.
    let mut buf = [0u8; 8];
    let n = value.len().min(8);
    buf[..n].copy_from_slice(&value[..n]);
    u64::from_be_bytes(buf) as f64
}

/// calcFraction4Datums.
// go: calcFraction4Datums L1
pub fn calc_fraction4_datums(lower: &Datum, upper: &Datum, value: &Datum) -> f64 {
    match value.kind() {
        KindFloat32 => calc_fraction(
            lower.get_float32() as f64,
            upper.get_float32() as f64,
            value.get_float32() as f64,
        ),
        KindFloat64 => calc_fraction(
            lower.get_float64(),
            upper.get_float64(),
            value.get_float64(),
        ),
        KindInt64 => calc_fraction(
            lower.get_int64() as f64,
            upper.get_int64() as f64,
            value.get_int64() as f64,
        ),
        KindUint64 => calc_fraction(
            lower.get_uint64() as f64,
            upper.get_uint64() as f64,
            value.get_uint64() as f64,
        ),
        KindMysqlDuration => calc_fraction(
            lower.get_mysql_duration().duration as f64,
            upper.get_mysql_duration().duration as f64,
            value.get_mysql_duration().duration as f64,
        ),
        KindMysqlDecimal | KindMysqlTime => calc_fraction(
            convert_datum_to_scalar(lower, 0),
            convert_datum_to_scalar(upper, 0),
            convert_datum_to_scalar(value, 0),
        ),
        KindBytes | KindString => {
            let common_pfx_len = common_prefix_length(&[lower.get_bytes(), upper.get_bytes()]);
            calc_fraction(
                convert_datum_to_scalar(lower, common_pfx_len),
                convert_datum_to_scalar(upper, common_pfx_len),
                convert_datum_to_scalar(value, common_pfx_len),
            )
        }
        _ => 0.5,
    }
}
