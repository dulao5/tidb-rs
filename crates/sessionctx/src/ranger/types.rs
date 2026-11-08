//! go-file: pkg/util/ranger/types.go
//! go-file: pkg/util/ranger/points.go
//!
//! `Range`, `Ranges` and `MutableRanges`.
//!
//! Mapping:
//! - Go `Ranges` is `[]*Range`; here `Vec<Range>` (`Ranges`), with Go's `nil` element of a
//!   `[]*Range` not representable (no Go caller stores one). `IntersectRange` returns
//!   `Option<Range>` for Go's `(*Range)(nil)`.
//! - `Collators` are `&'static dyn Collator` (the collation singletons). Go compares the
//!   interface values (`ran1.Collators[i] != ran2.Collators[i]`): same dynamic type and the
//!   same pointer; [`same_collator`] does the same.
//! - `Compare` errors: Go `(int, error)`; [`tidb_types::datum::Datum::compare`] returns the
//!   pair the same way.

// Datum kinds and Go names (mapping rule M-GONAME).
#![allow(non_upper_case_globals)]

use std::sync::Arc;

use tidb_base::Error;
use tidb_base::errors::RedactLogDisable;
use tidb_base::errors::RedactLogEnable;
use tidb_base::gofmt::Arg;
use tidb_base::gofmt::sprintf;
use tidb_base::gostd::time::Location;
use tidb_kv::Key;
use tidb_types::collate::Collator;
use tidb_types::collate::get_binary_collator_slice;
use tidb_types::context::Context as TypeContext;
use tidb_types::datum::*;

use crate::errctx;
use crate::planctx::PlanContext;

use super::context::RangerContext;

/// MutableRanges represents a range may change after it is created.
/// It's mainly designed for plan-cache, since some ranges in a cached plan have to be
/// rebuild when reusing.
pub trait MutableRanges: Send + Sync {
    /// Range returns the underlying range values.
    // glue: interface method of ranger.MutableRanges
    fn range(&self) -> Ranges;
    /// Rebuild rebuilds the underlying ranges again.
    // glue: interface method of ranger.MutableRanges
    fn rebuild(&mut self, sctx: &dyn PlanContext) -> Result<(), Error>;
    /// CloneForPlanCache clones the MutableRanges for plan cache.
    // glue: interface method of ranger.MutableRanges
    fn clone_for_plan_cache(&self) -> Option<Box<dyn MutableRanges>>;
}

/// Ranges implements the MutableRanges interface for range array.
pub type Ranges = Vec<Range>;

impl MutableRanges for Ranges {
    /// Range returns the range array.
    // go: Ranges.Range L1
    fn range(&self) -> Ranges {
        self.clone()
    }

    /// Rebuild rebuilds this range.
    // go: Ranges.Rebuild L1
    fn rebuild(&mut self, _sctx: &dyn PlanContext) -> Result<(), Error> {
        Ok(())
    }

    /// CloneForPlanCache clones the MutableRanges for plan cache.
    ///
    /// Go returns `nil` for a nil `Ranges`; an empty `Vec` is Go's non-nil empty slice, so
    /// this always returns a value.
    // go: Ranges.CloneForPlanCache L2 (nil Ranges is not distinguished from empty)
    fn clone_for_plan_cache(&self) -> Option<Box<dyn MutableRanges>> {
        let mut cloned: Vec<Range> = Vec::with_capacity(self.len());
        for r in self {
            cloned.push(r.clone_range());
        }
        Some(Box::new(cloned))
    }
}

/// MemUsage gets the memory usage of ranges.
// go: Ranges.MemUsage L1
pub fn ranges_mem_usage(rs: &[Range]) -> i64 {
    let mut sum = 0i64;
    for ran in rs {
        sum += ran.mem_usage();
    }
    sum
}

/// Range represents a range generated in physical plan building phase.
#[derive(Clone, Default)]
pub struct Range {
    /// Low value is exclusive.
    pub low_val: Vec<Datum>,
    /// High value is exclusive.
    pub high_val: Vec<Datum>,
    pub collators: Vec<&'static dyn Collator>,
    pub low_exclude: bool,
    pub high_exclude: bool,
}

impl std::fmt::Debug for Range {
    // glue: Debug through Go's String
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&String::from_utf8_lossy(&self.string()))
    }
}

/// Go interface equality of two collators: same dynamic type and same pointer.
// glue: Go interface value comparison of collate.Collator
pub fn same_collator(a: &'static dyn Collator, b: &'static dyn Collator) -> bool {
    let (ta, tb) = ((a as &dyn std::any::Any).type_id(), (b as &dyn std::any::Any).type_id());
    ta == tb && std::ptr::addr_eq(a as *const dyn Collator, b as *const dyn Collator)
}

impl Range {
    /// Width returns the width of this range.
    // go: Range.Width L1
    pub fn width(&self) -> usize {
        self.low_val.len()
    }

    /// Clone clones a Range.
    // go: Range.Clone L1
    pub fn clone_range(&self) -> Range {
        let mut new_range = Range {
            low_val: Vec::with_capacity(self.low_val.len()),
            high_val: Vec::with_capacity(self.high_val.len()),
            collators: Vec::new(),
            low_exclude: self.low_exclude,
            high_exclude: self.high_exclude,
        };
        for v in &self.low_val {
            new_range.low_val.push(v.clone());
        }
        for v in &self.high_val {
            new_range.high_val.push(v.clone());
        }
        new_range.collators.extend_from_slice(&self.collators);
        new_range
    }

    /// IsPoint returns if the range is a point.
    // go: Range.IsPoint L1
    pub fn is_point(&self, sctx: &RangerContext) -> bool {
        self.is_point_inner(&sctx.type_ctx, sctx.regard_null_as_point)
    }

    // go: Range.isPoint L1
    fn is_point_inner(&self, tc: &TypeContext, regard_null_as_point: bool) -> bool {
        if self.low_val.len() != self.high_val.len() {
            return false;
        }
        for i in 0..self.low_val.len() {
            let a = &self.low_val[i];
            let b = &self.high_val[i];
            if a.kind() == KindMinNotNull || b.kind() == KindMaxValue {
                return false;
            }
            let (cmp, err) = a.compare(tc, b, self.collators[i]);
            if err.is_err() {
                return false;
            }
            if cmp != 0 {
                return false;
            }

            if a.is_null() && b.is_null() {
                // [NULL, NULL]
                if !regard_null_as_point {
                    return false;
                }
            }
        }
        !self.low_exclude && !self.high_exclude
    }

    /// IsOnlyNull checks if the range has [NULL, NULL] or [NULL NULL, NULL NULL] range.
    // go: Range.IsOnlyNull L1
    pub fn is_only_null(&self) -> bool {
        for i in 0..self.low_val.len() {
            let a = &self.low_val[i];
            let b = &self.high_val[i];
            if !(a.is_null() && b.is_null()) {
                return false;
            }
        }
        true
    }

    /// IsPointNonNullable returns if the range is a point without NULL.
    // go: Range.IsPointNonNullable L1
    pub fn is_point_non_nullable(&self, tc: &TypeContext) -> bool {
        self.is_point_inner(tc, false)
    }

    /// IsPointNullable returns if the range is a point.
    // go: Range.IsPointNullable L1
    pub fn is_point_nullable(&self, tc: &TypeContext) -> bool {
        self.is_point_inner(tc, true)
    }

    /// IsFullRange check if the range is full scan range
    // go: Range.IsFullRange L1
    pub fn is_full_range(&self, unsigned_int_handle: bool) -> bool {
        if unsigned_int_handle {
            if self.low_val.len() != 1 || self.high_val.len() != 1 {
                return false;
            }
            return is_boundary_value(&self.low_val[0], true, true)
                && is_boundary_value(&self.high_val[0], true, false);
        }
        if self.low_val.len() != self.high_val.len() {
            return false;
        }
        for i in 0..self.low_val.len() {
            let left_is_boundary = is_boundary_value(&self.low_val[i], false, true);
            let left_is_null = self.low_val[i].is_null();
            let right_is_boundary = is_boundary_value(&self.high_val[i], false, false);
            let right_is_null = self.high_val[i].is_null();
            // treat [NULL, +inf), (-inf, NULL] as full range
            if (!left_is_boundary && !left_is_null)
                || (!right_is_boundary && !right_is_null)
                || (left_is_null && right_is_null)
            {
                return false;
            }
        }
        true
    }

    /// String implements the Stringer interface.
    /// don't use it in the product.
    // go: Range.String L1
    pub fn string(&self) -> Vec<u8> {
        self.string_inner(RedactLogDisable)
    }

    /// Redact is to print the range with redacting sensitive data.
    // go: Range.Redact L1
    pub fn redact(&self, redact: &str) -> Vec<u8> {
        self.string_inner(redact)
    }

    /// String implements the Stringer interface.
    // go: Range.string L1
    fn string_inner(&self, redact: &str) -> Vec<u8> {
        let mut low_strs: Vec<Vec<u8>> = Vec::with_capacity(self.low_val.len());
        for d in &self.low_val {
            low_strs.push(deal_with_redact(format_datum(d, true), redact));
        }
        let mut high_strs: Vec<Vec<u8>> = Vec::with_capacity(self.low_val.len());
        for d in &self.high_val {
            high_strs.push(deal_with_redact(format_datum(d, false), redact));
        }
        let (mut l, mut r) = (b"[", b"]");
        if self.low_exclude {
            l = b"(";
        }
        if self.high_exclude {
            r = b")";
        }
        let mut out = l.to_vec();
        out.extend(low_strs.join(&b' '));
        out.push(b',');
        out.extend(high_strs.join(&b' '));
        out.extend_from_slice(r);
        out
    }

    /// Encode encodes the range to its encoded value.
    ///
    /// Go reuses the capacity of `lowBuffer` / `highBuffer` (`buf[:0]`); the buffers are
    /// cleared and refilled here and returned on success. On a handled error Go returns
    /// `nil, nil, err`.
    // go: Range.Encode L1
    pub fn encode(
        &self,
        ec: &errctx::Context,
        loc: &Arc<Location>,
        mut low_buffer: Vec<u8>,
        mut high_buffer: Vec<u8>,
    ) -> Result<(Vec<u8>, Vec<u8>), Error> {
        low_buffer.clear();
        if let Err(err) = tidb_codec::codec::encode_key(loc, &mut low_buffer, &self.low_val) {
            ec.handle_error(err)?;
        }
        if self.low_exclude {
            low_buffer = Key(low_buffer).prefix_next().0;
        }
        high_buffer.clear();
        if let Err(err) = tidb_codec::codec::encode_key(loc, &mut high_buffer, &self.high_val) {
            ec.handle_error(err)?;
        }
        if !self.high_exclude {
            high_buffer = Key(high_buffer).prefix_next().0;
        }
        Ok((low_buffer, high_buffer))
    }

    /// Equal checks if two ranges are equal.
    ///
    /// Go also returns true for the same pointer and false when exactly one side is nil;
    /// [`range_equal_opt`] covers the nil cases.
    // go: Range.Equal L1
    pub fn equal(&self, other: &Range) -> bool {
        if std::ptr::eq(self, other) {
            return true;
        }
        if self.low_exclude != other.low_exclude || self.high_exclude != other.high_exclude {
            return false;
        }
        if self.low_val.len() != other.low_val.len()
            || self.high_val.len() != other.high_val.len()
        {
            return false;
        }
        for i in 0..self.low_val.len() {
            if !self.low_val[i].equals(Some(&other.low_val[i])) {
                return false;
            }
        }
        for i in 0..self.high_val.len() {
            if !self.high_val[i].equals(Some(&other.high_val[i])) {
                return false;
            }
        }
        true
    }

    /// PrefixEqualLen tells you how long the prefix of the range is a point.
    /// e.g. If this range is (1 2 3, 1 2 +inf), then the return value is 2.
    // go: Range.PrefixEqualLen L1
    pub fn prefix_equal_len(&self, tc: &TypeContext) -> Result<usize, Error> {
        // Here, len(ran.LowVal) always equal to len(ran.HighVal)
        for i in 0..self.low_val.len() {
            let (cmp, err) = self.low_val[i].compare(tc, &self.high_val[i], self.collators[i]);
            err?;
            if cmp != 0 {
                return Ok(i);
            }
        }
        Ok(self.low_val.len())
    }

    /// MemUsage gets the memory usage of range.
    // go: Range.MemUsage L1
    pub fn mem_usage(&self) -> i64 {
        // 16 is the size of Collator interface.
        let mut sum = EmptyRangeSize + self.collators.len() as i64 * 16;
        for val in &self.low_val {
            sum += val.mem_usage();
        }
        for val in &self.high_val {
            sum += val.mem_usage();
        }
        // We ignore size of collator currently.
        sum
    }

    /// Subset for Range type, check if range(ran)  is a subset of another range(otherRange).
    /// This is done by:
    ///   - Both ran and otherRange have the same collators. This is not needed for the
    ///     current code path. But, it is used here for future use of the function.
    ///   - Checking if the lower/upper bound of otherRange covers the corresponding
    ///     lower/upper bound of ran. Thus include checking open/closed inetrvals.
    // go: Range.Subset L1
    pub fn subset(&self, tc: &TypeContext, other_range: &Range) -> bool {
        if self.low_val.len() < other_range.low_val.len() {
            return false;
        }

        if !check_collators(self, other_range, other_range.low_val.len()) {
            return false;
        }

        // Either otherRange is closed or both ranges have the same open/close setting.
        let low_exclude_ok = !other_range.low_exclude || self.low_exclude == other_range.low_exclude;
        let high_exclude_ok =
            !other_range.high_exclude || self.high_exclude == other_range.high_exclude;
        if !low_exclude_ok || !high_exclude_ok {
            return false;
        }

        prefix(tc, &other_range.low_val, &self.low_val, other_range.low_val.len(), &self.collators)
            && prefix(
                tc,
                &other_range.high_val,
                &self.high_val,
                other_range.low_val.len(),
                &self.collators,
            )
    }

    /// IntersectRange computes intersection between two ranges. err is set of something went
    /// wrong during comparison.
    ///
    /// Go returns `(&Range{}, err)` on an error; the error alone is returned here.
    // go: Range.IntersectRange L1
    pub fn intersect_range(&self, tc: &TypeContext, other_range: &Range) -> Result<Option<Range>, Error> {
        let intersect_length = self.low_val.len().max(other_range.low_val.len());
        let mut result = Range {
            low_val: Vec::with_capacity(intersect_length),
            high_val: Vec::with_capacity(intersect_length),
            collators: Vec::with_capacity(intersect_length),
            low_exclude: false,
            high_exclude: false,
        };

        let mut other_range_more_granual = false;
        if self.low_val.len() > other_range.low_val.len() {
            result.collators = self.collators.clone();
        } else {
            result.collators = other_range.collators.clone();
            other_range_more_granual = true;
        }

        let low_vs_high = compare_lexicographically(
            tc,
            &self.low_val,
            &other_range.high_val,
            &result.collators,
            self.low_exclude,
            other_range.high_exclude,
            true,
            false,
        )?;
        if low_vs_high == 1 {
            return Ok(None);
        }

        let low_vs_high = compare_lexicographically(
            tc,
            &other_range.low_val,
            &self.high_val,
            &result.collators,
            other_range.low_exclude,
            self.high_exclude,
            true,
            false,
        )?;
        if low_vs_high == 1 {
            return Ok(None);
        }

        let low_vs_low = compare_lexicographically(
            tc,
            &self.low_val,
            &other_range.low_val,
            &result.collators,
            self.low_exclude,
            other_range.low_exclude,
            true,
            true,
        )?;
        if low_vs_low == -1 || (low_vs_low == 0 && other_range_more_granual) {
            result.low_val = other_range.low_val.clone();
            result.low_exclude = other_range.low_exclude;
        } else {
            result.low_val = self.low_val.clone();
            result.low_exclude = self.low_exclude;
        }

        let high_vs_high = compare_lexicographically(
            tc,
            &self.high_val,
            &other_range.high_val,
            &result.collators,
            self.high_exclude,
            other_range.high_exclude,
            false,
            false,
        )?;
        if high_vs_high == 1 || (high_vs_high == 0 && other_range_more_granual) {
            result.high_val = other_range.high_val.clone();
            result.high_exclude = other_range.high_exclude;
        } else {
            result.high_val = self.high_val.clone();
            result.high_exclude = self.high_exclude;
        }
        Ok(Some(result))
    }
}

/// `Range.Equal` with Go's nil receivers: equal when both are nil, unequal when one is.
// glue: nil cases of Range.Equal
pub fn range_equal_opt(ran: Option<&Range>, other: Option<&Range>) -> bool {
    match (ran, other) {
        (None, None) => true,
        (Some(a), Some(b)) => a.equal(b),
        _ => false,
    }
}

/// HasFullRange checks if any range in the slice is a full range.
// go: HasFullRange L1
pub fn has_full_range(ranges: &[Range], unsigned_int_handle: bool) -> bool {
    for ran in ranges {
        if ran.is_full_range(unsigned_int_handle) {
            return true;
        }
    }
    false
}

// go: dealWithRedact L1
fn deal_with_redact(input: Vec<u8>, redact: &str) -> Vec<u8> {
    if input == b"-inf" || input == b"+inf" {
        return input;
    }
    if redact == RedactLogDisable {
        return input;
    } else if redact == RedactLogEnable {
        return b"?".to_vec();
    }
    sprintf("‹%s›", &[Arg::go_string(input)])
}

/// EmptyRangeSize is the size of empty range: Go `unsafe.Sizeof(Range{})` on a 64-bit
/// target (three slice headers of 24 bytes and two bools, padded to 8).
pub const EmptyRangeSize: i64 = 80;

// go: isBoundaryValue L1
fn is_boundary_value(d: &Datum, unsigned_int_handle: bool, is_left_side: bool) -> bool {
    let is_right_side = !is_left_side;
    match d.kind() {
        KindMinNotNull => is_left_side,  // -inf
        KindMaxValue => is_right_side,   // +inf
        KindInt64 => {
            let v = d.get_int64();
            (v == i64::MIN && is_left_side) // -inf
                || (v == i64::MAX && is_right_side) // +inf
        }
        KindUint64 => {
            let v = d.get_uint64();
            (v == 0 && unsigned_int_handle && is_left_side) // 0
                || (v == u64::MAX && is_right_side) // +inf
        }
        // for other types, no concept of boundary value
        _ => false,
    }
}

// go: formatDatum L1
fn format_datum(d: &Datum, is_left_side: bool) -> Vec<u8> {
    match d.kind() {
        KindNull => return b"NULL".to_vec(),
        KindMinNotNull => return b"-inf".to_vec(),
        KindMaxValue => return b"+inf".to_vec(),
        KindInt64 => {
            let v = d.get_int64();
            match v {
                i64::MIN => {
                    if is_left_side {
                        return b"-inf".to_vec();
                    }
                }
                i64::MAX => {
                    if !is_left_side {
                        return b"+inf".to_vec();
                    }
                }
                _ => {}
            }
            return v.to_string().into_bytes();
        }
        KindUint64 => {
            let v = d.get_uint64();
            if v == u64::MAX && !is_left_side {
                return b"+inf".to_vec();
            }
            return v.to_string().into_bytes();
        }
        KindBytes | KindString => return sprintf("%q", &[d.get_value().arg()]),
        KindMysqlEnum | KindMysqlSet | KindMysqlJSON | KindBinaryLiteral | KindMysqlBit => {
            return sprintf("\"%v\"", &[d.get_value().arg()]);
        }
        _ => {}
    }
    sprintf("%v", &[d.get_value().arg()])
}

/// extendBound extends a partial bound slice by appending "infinite" sentinel values.
/// It's used when constructing multi-column index scan ranges.
///
/// The logic depends on whether the bound is a lower or upper bound,
/// and whether it's open (exclusive) or closed (inclusive):
///   - Lower Bound (`low == true`):
///   - Open   -> append +∞ (represented by MaxInt64): exclude current value, start just above
///   - Closed -> append –∞ (represented by MinInt64): include all lower values
///   - Upper Bound (`low == false`):
///   - Open   -> append –∞ (represented by MinInt64): exclude current value, stop just below
///   - Closed -> append +∞ (represented by MaxInt64): include all higher values
///
/// This padding is essential in multi-column indexes when only a prefix of the columns
/// is constrained. The remaining columns are filled with ±∞ to form complete range bounds.
// go: extendBound L1
fn extend_bound(bound: &mut Vec<Datum>, low_index: usize, high_index: usize, low: bool, open: bool) {
    for _ in low_index..high_index {
        if low {
            if open {
                // Open lower bound → +∞ (exclude the current value)
                bound.push(max_value_datum());
            } else {
                // Closed lower bound → –∞ (include all lower values)
                bound.push(min_not_null_datum());
            }
        } else if open {
            // Open upper bound → –∞ (exclude the current value)
            bound.push(min_not_null_datum());
        } else {
            // Closed upper bound → +∞ (include all higher values)
            bound.push(max_value_datum());
        }
    }
}

/// compareLexicographically compares two bounds from two ranges and returns 0, 1, -1
/// for equal, greater than or less than respectively. It gets the two bounds,
/// collations and if each bound is open (open1, open2) or closed. In addition,
/// it also gets if each bound is lower or upper (low1, low2).
/// Lower bounds logically can be extended with -infinity and upper bounds can be extended
/// with +infinity.
///
/// Go indexes `collators[i]` for every column of the longer bound and panics when the
/// slice is shorter; the Rust index panics the same way.
// go: compareLexicographically L1
#[allow(clippy::too_many_arguments)]
fn compare_lexicographically(
    tc: &TypeContext,
    bound1: &[Datum],
    bound2: &[Datum],
    collators: &[&'static dyn Collator],
    open1: bool,
    open2: bool,
    low1: bool,
    low2: bool,
) -> Result<isize, Error> {
    let n1 = bound1.len();
    let n2 = bound2.len();
    let mut local1: std::borrow::Cow<'_, [Datum]> = bound1.into();
    let mut local2: std::borrow::Cow<'_, [Datum]> = bound2.into();

    if n1 < n2 {
        // Copy bound1 before extending
        let mut v = bound1.to_vec();
        extend_bound(&mut v, n1, n2, low1, open1);
        local1 = v.into();
    } else if n2 < n1 {
        // Copy bound2 before extending
        let mut v = bound2.to_vec();
        extend_bound(&mut v, n2, n1, low2, open2);
        local2 = v.into();
    }

    let n = n1.max(n2);
    for i in 0..n {
        let (cmp, err) = local1[i].compare(tc, &local2[i], collators[i]);
        err?;
        if cmp != 0 {
            return Ok(cmp);
        }
    }

    Ok(if !open1 && !open2 {
        0
    } else if open1 == open2 {
        if low1 == low2 {
            0
        } else if low1 {
            1
        } else {
            -1
        }
    } else if open1 {
        if low1 { 1 } else { -1 }
    } else {
        // Same as case open2:
        if low2 { -1 } else { 1 }
    })
}

/// Check if a list of Datum is a prefix of another list of Datum. This is useful for
/// checking if lower/upper bound of a range is a subset of another.
// go: prefix L1
fn prefix(
    tc: &TypeContext,
    super_value: &[Datum],
    sup_value: &[Datum],
    length: usize,
    collators: &[&'static dyn Collator],
) -> bool {
    for i in 0..length {
        let (cmp, err) = super_value[i].compare(tc, &sup_value[i], collators[i]);
        if err.is_err() || cmp != 0 {
            return false;
        }
    }
    true
}

/// Subset checks if a list of ranges(rs) is a subset of another list of
/// ranges(superRanges). This is true if every range in the first list is a subset of any
/// range in the second list. Also, we check if all elements of superRanges are covered.
// go: Ranges.Subset L1
pub fn ranges_subset(rs: &[Range], tc: &TypeContext, super_ranges: &[Range]) -> bool {
    let mut super_ranges_covered = vec![false; super_ranges.len()];
    if rs.is_empty() {
        return super_ranges.is_empty();
    } else if super_ranges.is_empty() {
        // unrestricted superRanges and restricted rs
        return true;
    }

    for sub_range in rs {
        let mut subset = false;
        for (i, super_range) in super_ranges.iter().enumerate() {
            if sub_range.subset(tc, super_range) {
                subset = true;
                super_ranges_covered[i] = true;
                break;
            }
        }
        if !subset {
            return false;
        }
    }
    for covered in super_ranges_covered {
        if !covered {
            return false;
        }
    }

    true
}

// go: checkCollators L1
fn check_collators(ran1: &Range, ran2: &Range, length: usize) -> bool {
    // Make sure both ran and superRange have the same collations.
    // The current code path for this function always will have same collation
    // for ran and superRange. It is added here for future
    // use of the function.
    for i in 0..length {
        if !same_collator(ran1.collators[i], ran2.collators[i]) {
            return false;
        }
    }
    true
}

/// IntersectRanges computes pairwise intersection between each element in rs and
/// otherRangeList.
///
/// Go returns `nil` on a collator mismatch or an error and a non-nil (possibly empty)
/// slice otherwise; `None` is Go's nil.
// go: Ranges.IntersectRanges L1
pub fn ranges_intersect_ranges(rs: &[Range], tc: &TypeContext, other_ranges: &[Range]) -> Option<Ranges> {
    let mut result: Ranges = Vec::new();
    for rs_range in rs {
        for other_range in other_ranges {
            let subset_length = rs_range.low_val.len().min(other_range.low_val.len());
            if !check_collators(rs_range, other_range, subset_length) {
                return None;
            }
            let Ok(one_intersection) = rs_range.intersect_range(tc, other_range) else {
                return None;
            };
            if let Some(one_intersection) = one_intersection {
                result.push(one_intersection);
            }
        }
    }
    Some(result)
}

// ---------------------------------------------------------------- points.go (full ranges)

/// FullIntRange is used for table range. Since table range cannot accept MaxValueDatum as
/// the max value. So we need to set it to MaxInt64.
// go: FullIntRange L1
pub fn full_int_range(is_unsigned: bool) -> Ranges {
    if is_unsigned {
        return vec![Range {
            low_val: vec![new_uint_datum(0)],
            high_val: vec![new_uint_datum(u64::MAX)],
            collators: get_binary_collator_slice(1),
            ..Default::default()
        }];
    }
    vec![Range {
        low_val: vec![new_int_datum(i64::MIN)],
        high_val: vec![new_int_datum(i64::MAX)],
        collators: get_binary_collator_slice(1),
        ..Default::default()
    }]
}

/// FullRange is [null, +∞) for Range.
// go: FullRange L1
pub fn full_range() -> Ranges {
    vec![Range {
        low_val: vec![Datum::default()],
        high_val: vec![max_value_datum()],
        collators: get_binary_collator_slice(1),
        ..Default::default()
    }]
}

/// FullNotNullRange is (-∞, +∞) for Range.
// go: FullNotNullRange L1
pub fn full_not_null_range() -> Ranges {
    vec![Range {
        low_val: vec![min_not_null_datum()],
        high_val: vec![max_value_datum()],
        collators: get_binary_collator_slice(1),
        ..Default::default()
    }]
}
