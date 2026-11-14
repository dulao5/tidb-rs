//! Record key ranges of integer-handle tables (`t{table_id}_r{handle}`).

use std::ops::Bound;

use poc_kv::KeyRange;
use tidb_query_datatype::codec::table;

/// A range of integer handles. `Unbounded` on either side means `i64::MIN` / `i64::MAX`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HandleRange {
    pub low: Bound<i64>,
    pub high: Bound<i64>,
}

impl HandleRange {
    pub fn new(low: Bound<i64>, high: Bound<i64>) -> HandleRange {
        HandleRange { low, high }
    }

    /// Every handle (Go: `ranger.FullIntRange`).
    pub fn full() -> HandleRange {
        HandleRange::new(Bound::Unbounded, Bound::Unbounded)
    }

    /// The single handle `h`.
    pub fn point(h: i64) -> HandleRange {
        HandleRange::new(Bound::Included(h), Bound::Included(h))
    }

    /// `lo..=hi`.
    pub fn inclusive(lo: i64, hi: i64) -> HandleRange {
        HandleRange::new(Bound::Included(lo), Bound::Included(hi))
    }

    /// Smallest included handle, or `None` if the range is empty from below.
    fn first(&self) -> Option<i64> {
        match self.low {
            Bound::Unbounded => Some(i64::MIN),
            Bound::Included(h) => Some(h),
            Bound::Excluded(h) => h.checked_add(1),
        }
    }

    /// Largest included handle, or `None` if the range is empty from above.
    fn last(&self) -> Option<i64> {
        match self.high {
            Bound::Unbounded => Some(i64::MAX),
            Bound::Included(h) => Some(h),
            Bound::Excluded(h) => h.checked_sub(1),
        }
    }

    /// The inclusive bounds `(first, last)`, or `None` if the range is empty.
    pub fn to_inclusive(&self) -> Option<(i64, i64)> {
        let (first, last) = (self.first()?, self.last()?);
        (first <= last).then_some((first, last))
    }
}

/// A non-empty inclusive range of integer handle *values*: Go's `ranger.Range` over an int
/// handle, whose datums are `KindInt64`, or `KindUint64` when the handle column is UNSIGNED
/// (`ranger.FullIntRange(isUnsigned)`). Ranges of one read are sorted by value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IntRange {
    Signed(i64, i64),
    Unsigned(u64, u64),
}

impl IntRange {
    /// The handles the range covers. The key of a row is the int64 encoding of its handle;
    /// for an UNSIGNED handle that is the bit pattern of the value (Go `encodeHandleKey` uses
    /// `Datum.GetInt64()`), so `Unsigned(lo, hi)` must not straddle `i64::MAX` (see
    /// [`split_ranges_across_int64_boundary`]).
    pub fn to_handle_range(self) -> HandleRange {
        match self {
            IntRange::Signed(lo, hi) => HandleRange::inclusive(lo, hi),
            IntRange::Unsigned(lo, hi) => {
                debug_assert!(
                    lo > i64::MAX as u64 || hi <= i64::MAX as u64,
                    "unsigned range [{lo},{hi}] straddles the int64 boundary"
                );
                HandleRange::inclusive(lo as i64, hi as i64)
            }
        }
    }
}

impl std::fmt::Display for IntRange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IntRange::Signed(lo, hi) => write!(f, "[{lo},{hi}]"),
            IntRange::Unsigned(lo, hi) => write!(f, "[{lo},{hi}]"),
        }
    }
}

// go: distsql.SplitRangesAcrossInt64Boundary (pkg/distsql/request_builder.go)
/// Splits `ranges` into two groups: the values `<= i64::MAX` ("signed") and the values
/// `> i64::MAX` ("unsigned"), splitting a range that straddles the boundary. Row keys encode
/// a handle as an int64, so an UNSIGNED value `>= 2^63` sorts *before* zero in key order.
///
/// Without `keep_order` both groups go into one request, the unsigned ones first (which is key
/// order); with `keep_order` the first group is read before the second in a separate request:
/// signed then unsigned ascending, unsigned then signed descending. Signed (`KindInt64`)
/// ranges are returned unchanged. (Go's common-handle case does not arise: these are int
/// handles only.)
pub fn split_ranges_across_int64_boundary(
    ranges: &[IntRange],
    keep_order: bool,
    desc: bool,
) -> (Vec<IntRange>, Vec<IntRange>) {
    const MAX_I64: u64 = i64::MAX as u64;
    let hi_of = |r: &IntRange| match *r {
        IntRange::Unsigned(_, hi) => hi,
        IntRange::Signed(..) => unreachable!("mixed signed and unsigned ranges"),
    };
    if ranges.is_empty() || matches!(ranges[0], IntRange::Signed(..)) {
        return (ranges.to_vec(), Vec::new());
    }
    let idx = ranges.partition_point(|r| hi_of(r) <= MAX_I64);
    if idx == ranges.len() {
        return (ranges.to_vec(), Vec::new());
    }
    let IntRange::Unsigned(lo, hi) = ranges[idx] else {
        unreachable!("mixed signed and unsigned ranges")
    };
    let (signed, unsigned) = if lo > MAX_I64 {
        (ranges[..idx].to_vec(), ranges[idx..].to_vec())
    } else {
        // Straddles the boundary. Go skips an empty half (`(MaxInt64, ..` / `.., MaxInt64+1)`
        // with an exclusive bound); inclusive bounds never leave one empty.
        let mut signed = ranges[..idx].to_vec();
        signed.push(IntRange::Unsigned(lo, MAX_I64));
        let mut unsigned = vec![IntRange::Unsigned(MAX_I64 + 1, hi)];
        unsigned.extend_from_slice(&ranges[idx + 1..]);
        (signed, unsigned)
    };
    if !keep_order {
        let mut all = unsigned;
        all.extend(signed);
        return (all, Vec::new());
    }
    if desc {
        (unsigned, signed)
    } else {
        (signed, unsigned)
    }
}

/// Raw key of the row with integer handle `handle` (Go: `tablecodec.EncodeRowKeyWithHandle`).
pub fn row_key(table_id: i64, handle: i64) -> Vec<u8> {
    table::encode_row_key(table_id, handle)
}

/// Key range covering `range` in table `table_id`, or `None` if `range` is empty. As in Go's
/// `distsql.TableHandleRangesToKVRanges`, the end key is the row key of `last + 1`, or the
/// prefix-next of the row key of `i64::MAX` when `last` is `i64::MAX`.
pub fn record_range(table_id: i64, range: HandleRange) -> Option<KeyRange> {
    let (first, last) = range.to_inclusive()?;
    let start = row_key(table_id, first);
    let end = match last.checked_add(1) {
        Some(next) => row_key(table_id, next),
        None => prefix_next(&row_key(table_id, i64::MAX)),
    };
    Some(KeyRange::new(start, end))
}

/// Key ranges for `ranges`, sorted, with empty ranges dropped and overlapping or adjacent
/// ones merged (a coprocessor request needs sorted, non-overlapping ranges).
pub fn record_ranges(table_id: i64, ranges: &[HandleRange]) -> Vec<KeyRange> {
    let mut bounds: Vec<(i64, i64)> = ranges.iter().filter_map(|r| r.to_inclusive()).collect();
    bounds.sort_unstable();
    let mut merged: Vec<(i64, i64)> = Vec::with_capacity(bounds.len());
    for (lo, hi) in bounds {
        match merged.last_mut() {
            Some(last) if lo <= last.1.saturating_add(1) => last.1 = last.1.max(hi),
            _ => merged.push((lo, hi)),
        }
    }
    merged
        .into_iter()
        .filter_map(|(lo, hi)| record_range(table_id, HandleRange::inclusive(lo, hi)))
        .collect()
}

fn prefix_next(key: &[u8]) -> Vec<u8> {
    let mut out = key.to_vec();
    while let Some(last) = out.last_mut() {
        if *last == u8::MAX {
            out.pop();
        } else {
            *last += 1;
            return out;
        }
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn split_across_int64_boundary() {
        const B: u64 = i64::MAX as u64;
        let u = IntRange::Unsigned;
        let full = [u(0, u64::MAX)];
        // The full UNSIGNED range (`ranger.FullIntRange(true)`).
        assert_eq!(
            split_ranges_across_int64_boundary(&full, true, false),
            (vec![u(0, B)], vec![u(B + 1, u64::MAX)])
        );
        assert_eq!(
            split_ranges_across_int64_boundary(&full, true, true),
            (vec![u(B + 1, u64::MAX)], vec![u(0, B)])
        );
        assert_eq!(
            split_ranges_across_int64_boundary(&full, false, false),
            (vec![u(B + 1, u64::MAX), u(0, B)], vec![])
        );
        // Entirely on one side.
        assert_eq!(
            split_ranges_across_int64_boundary(&[u(1, 5)], true, false),
            (vec![u(1, 5)], vec![])
        );
        assert_eq!(
            split_ranges_across_int64_boundary(&[u(1, 5), u(B + 2, B + 9)], true, false),
            (vec![u(1, 5)], vec![u(B + 2, B + 9)])
        );
        assert_eq!(
            split_ranges_across_int64_boundary(&[u(B + 2, B + 9)], true, false),
            (vec![], vec![u(B + 2, B + 9)])
        );
        // Several ranges around a straddling one.
        assert_eq!(
            split_ranges_across_int64_boundary(
                &[u(1, 2), u(B - 1, B + 1), u(B + 5, B + 6)],
                true,
                true
            ),
            (
                vec![u(B + 1, B + 1), u(B + 5, B + 6)],
                vec![u(1, 2), u(B - 1, B)]
            )
        );
        // Exactly at the boundary.
        assert_eq!(
            split_ranges_across_int64_boundary(&[u(B, B + 1)], true, false),
            (vec![u(B, B)], vec![u(B + 1, B + 1)])
        );
        // Signed ranges are untouched.
        let s = [IntRange::Signed(i64::MIN, i64::MAX)];
        assert_eq!(
            split_ranges_across_int64_boundary(&s, true, true),
            (s.to_vec(), vec![])
        );
        assert_eq!(
            split_ranges_across_int64_boundary(&[], true, false),
            (vec![], vec![])
        );
    }

    #[test]
    fn unsigned_values_map_to_their_bit_pattern() {
        assert_eq!(
            IntRange::Unsigned(1 << 63, u64::MAX).to_handle_range(),
            HandleRange::inclusive(i64::MIN, -1)
        );
        // The end key of `u64::MAX` is the row key of handle 0, as Go's `PrefixNext`.
        assert_eq!(
            record_range(7, IntRange::Unsigned(1 << 63, u64::MAX).to_handle_range()),
            Some(KeyRange::new(row_key(7, i64::MIN), row_key(7, 0)))
        );
    }

    #[test]
    fn row_keys_match_go() {
        // Printed by Go's `codec.EncodeInt` composed as `tablecodec` does.
        assert_eq!(
            row_key(114, 1),
            hex("7480000000000000725f728000000000000001")
        );
        assert_eq!(
            row_key(114, -5),
            hex("7480000000000000725f727ffffffffffffffb")
        );
    }

    #[test]
    fn bounds() {
        let k = |h| row_key(7, h);
        assert_eq!(
            record_range(7, HandleRange::inclusive(1, 100)),
            Some(KeyRange::new(k(1), k(101)))
        );
        assert_eq!(
            record_range(
                7,
                HandleRange::new(Bound::Excluded(1), Bound::Excluded(100))
            ),
            Some(KeyRange::new(k(2), k(100)))
        );
        assert_eq!(
            record_range(7, HandleRange::point(5)),
            Some(KeyRange::new(k(5), k(6)))
        );
        // Empty ranges.
        assert_eq!(record_range(7, HandleRange::inclusive(3, 2)), None);
        assert_eq!(
            record_range(7, HandleRange::new(Bound::Excluded(3), Bound::Excluded(4))),
            None
        );
        assert_eq!(
            record_range(
                7,
                HandleRange::new(Bound::Excluded(i64::MAX), Bound::Unbounded)
            ),
            None
        );
        assert_eq!(
            record_range(
                7,
                HandleRange::new(Bound::Unbounded, Bound::Excluded(i64::MIN))
            ),
            None
        );
    }

    #[test]
    fn full_range_covers_exactly_the_record_space() {
        let full = record_range(7, HandleRange::full()).unwrap();
        assert_eq!(full.start, row_key(7, i64::MIN));
        // The end is the prefix-next of `t7_r`: every record of table 7 is below it, table
        // 7's index keys (`t7_i...`) sort before the start, and table 8 after the end.
        let mut prefix = table::encode_row_key(7, 0);
        prefix.truncate(table::PREFIX_LEN);
        assert_eq!(full.end, prefix_next(&prefix));
        assert!(full.start.starts_with(&prefix));
        assert!(row_key(7, i64::MAX) < full.end);
        assert!(row_key(8, i64::MIN) > full.end);
    }

    #[test]
    fn ranges_are_sorted_and_merged() {
        let k = |h| row_key(9, h);
        let ranges = record_ranges(
            9,
            &[
                HandleRange::inclusive(50, 60),
                HandleRange::inclusive(1, 10),
                HandleRange::inclusive(11, 20), // adjacent to 1..=10
                HandleRange::inclusive(55, 70), // overlaps 50..=60
                HandleRange::inclusive(5, 3),   // empty
                HandleRange::point(100),
            ],
        );
        assert_eq!(
            ranges,
            [
                KeyRange::new(k(1), k(21)),
                KeyRange::new(k(50), k(71)),
                KeyRange::new(k(100), k(101)),
            ]
        );
        // Merging next to i64::MAX must not overflow.
        let r = record_ranges(
            9,
            &[
                HandleRange::new(Bound::Included(i64::MAX - 1), Bound::Unbounded),
                HandleRange::point(i64::MAX),
            ],
        );
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].start, k(i64::MAX - 1));
    }
}
