//! Record key ranges of integer-handle tables (`t{table_id}_r{handle}`).

use std::ops::Bound;

use tidb_kv::KeyRange;
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
