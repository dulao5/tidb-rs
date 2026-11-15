//! go-file: pkg/util/ranger/ranger.go
//!
//! Converting points to ranges, building the ranges of one column / of the CNF access
//! conditions of an index, `UnionRanges`, the prefix-index cut, and printing ranges as SQL
//! conditions.
//!
//! Mapping (D10.4 spec §3):
//! - `Ranges` is `Vec<Range>` (D00.1): Go's batch allocation of the emitted ranges'
//!   backing arrays (`lowValBuf[i : i+1 : i+1]`, kept apart so a later `append` cannot
//!   overwrite a neighbour) is a separate `Vec` per range here, which has the same effect.
//! - `UnionRanges` sorts and merges by value: Go writes the merged high bound into the
//!   caller's `*Range` objects and reuses the input slice; every caller replaces its ranges
//!   with the result, so the effect is the same.
//! - `points2EqOrInCond` builds `in(...)`: the IN builtin is D08.9, so it returns the
//!   boundary error `blocked-by:D08.9` until that block lands (spec §5).

// AST function names and type codes keep their Go names (M-GONAME).
#![allow(non_upper_case_globals)]

use std::sync::Arc;

use tidb_base::Error;
use tidb_base::gostd::sort::sort;
use tidb_base::gostd::strconv::format_float;
use tidb_base::mysql;
use tidb_expression::scalar_function::{ast_func as ast, new_function};
use tidb_expression::{Column, Constant, Expr, ExprRef};
use tidb_kv::Key;
use tidb_parser::gocharset::{CharsetASCII, CharsetBin, CollationBin};
use tidb_parser::types::ETString;
use tidb_parser::types::{FieldType, UnspecifiedLength};
use tidb_sessionctx::exprctx::BuildContext;
use tidb_sessionctx::ranger::context::RangerContext;
use tidb_sessionctx::ranger::types::{EmptyRangeSize, Range, Ranges, full_int_range};
use tidb_sessionctx::ranger::{full_not_null_range, full_range};
use tidb_sessionctx::stmtctx::StatementContext;
use tidb_types::collate::{self, Collator};
use tidb_types::datum::*;
use tidb_types::enum_type::parse_enum_value;
use tidb_types::errors::{
    ErrDataTooLong, ErrOverflow, ErrTruncated, ErrWarnDataOutOfRange, ErrWrongValue,
};
use tidb_types::field_type::{new_field_type, new_field_type_with_collation};

use super::detacher::RangeDetacher;
use super::points::{Builder, Point, get_full_range};

/// `terror.ErrorEqual(err, target)`.
// glue: terror.ErrorEqual on a returned error
fn err_is(err: &Error, target: &Error) -> bool {
    tidb_base::errors::error_equal(Some(err), Some(target))
}

/// `codec.EncodeKey(loc, nil, d...)` passed through `ec.HandleError`: Go keeps the bytes
/// encoded so far when the error is handled.
// glue: codec.EncodeKey + errctx.Context.HandleError of validInterval / UnionRanges
fn encode_key_handled(sctx: &RangerContext, vals: &[Datum]) -> Result<Vec<u8>, Error> {
    let mut b = Vec::new();
    if let Err(err) = tidb_codec::codec::encode_key(&sctx.type_ctx.location(), &mut b, vals) {
        sctx.err_ctx.handle_error(err)?;
    }
    Ok(b)
}

// go: validInterval L1
fn valid_interval(sctx: &RangerContext, low: &Point, high: &Point) -> Result<bool, Error> {
    let mut l = encode_key_handled(sctx, std::slice::from_ref(&low.value))?;
    if low.excl {
        l = Key(l).prefix_next().0;
    }
    let mut r = encode_key_handled(sctx, std::slice::from_ref(&high.value))?;
    if !high.excl {
        r = Key(r).prefix_next().0;
    }
    Ok(l < r)
}

/// convertPointsInPlace does some preprocessing on rangePoints to make them ready to build ranges. It converts
/// points to the specified type in place, validates intervals, and compacts valid intervals to the front of rangePoints.
// go: convertPointsInPlace L1
fn convert_points_in_place(
    sctx: &RangerContext,
    mut range_points: Vec<Point>,
    new_tp: &FieldType,
    skip_null: bool,
    table_range: bool,
) -> Result<Vec<Point>, Error> {
    let mut i = 0usize;
    let num_points = range_points.len();
    let (mut min_value_datum, mut max_value_datum) = (Datum::default(), Datum::default());
    if table_range {
        // Currently, table's kv range cannot accept encoded value of MaxValueDatum. we need to convert it.
        let is_unsigned = mysql::has_unsigned_flag(new_tp.get_flag());
        if is_unsigned {
            min_value_datum.set_uint64(0);
            max_value_datum.set_uint64(u64::MAX);
        } else {
            min_value_datum.set_int64(i64::MIN);
            max_value_datum.set_int64(i64::MAX);
        }
    }
    let mut j = 0usize;
    while j < num_points {
        {
            let start_point = &mut range_points[j];
            convert_point_in_place(sctx, start_point, new_tp)?;
            if table_range {
                if start_point.value.kind() == KindNull {
                    start_point.value = min_value_datum.clone();
                    start_point.excl = false;
                } else if start_point.value.kind() == KindMinNotNull {
                    start_point.value = min_value_datum.clone();
                }
            }
        }
        {
            let end_point = &mut range_points[j + 1];
            convert_point_in_place(sctx, end_point, new_tp)?;
            if table_range && end_point.value.kind() == KindMaxValue {
                end_point.value = max_value_datum.clone();
            }
        }
        if skip_null && range_points[j + 1].value.kind() == KindNull {
            j += 2;
            continue;
        }
        let less = valid_interval(sctx, &range_points[j], &range_points[j + 1])?;
        if !less {
            j += 2;
            continue;
        }
        range_points.swap(i, j);
        range_points.swap(i + 1, j + 1);
        i += 2;
        j += 2;
    }
    range_points.truncate(i);
    Ok(range_points)
}

/// estimateMemUsageForPoints2Ranges estimates the memory usage of ranges converted from points.
// go: estimateMemUsageForPoints2Ranges L1
fn estimate_mem_usage_for_points2_ranges(range_points: &[Point]) -> i64 {
    // 16 is the size of Range.Collators
    (EmptyRangeSize + 16) * (range_points.len() as i64) / 2
        + get_points_total_datum_size(range_points)
}

/// points2Ranges build index ranges from range points.
/// Only one column is built there. If there're multiple columns, use appendPoints2Ranges.
/// rangeMaxSize is the max memory limit for ranges. O indicates no memory limit.
/// If the second return value is true, it means that the estimated memory usage of ranges exceeds rangeMaxSize and it falls back to full range.
// go: points2Ranges L1
pub(crate) fn points2_ranges(
    sctx: &RangerContext,
    range_points: Vec<Point>,
    new_tp: &FieldType,
    range_max_size: i64,
) -> Result<(Ranges, bool), Error> {
    let range_points = convert_points_in_place(
        sctx,
        range_points,
        new_tp,
        mysql::has_not_null_flag(new_tp.get_flag()),
        false,
    )?;
    // Estimate whether rangeMaxSize will be exceeded first before converting points to ranges.
    if range_max_size > 0 && estimate_mem_usage_for_points2_ranges(&range_points) > range_max_size {
        let full_range = if mysql::has_not_null_flag(new_tp.get_flag()) {
            full_not_null_range()
        } else {
            full_range()
        };
        return Ok((full_range, true));
    }
    let range_count = range_points.len() / 2;
    let mut ranges: Ranges = Vec::with_capacity(range_count);
    let range_collator = collate::get_collator(new_tp.get_collate());
    let mut it = range_points.into_iter();
    while let (Some(start_point), Some(end_point)) = (it.next(), it.next()) {
        ranges.push(Range {
            low_val: vec![start_point.value],
            low_exclude: start_point.excl,
            high_val: vec![end_point.value],
            high_exclude: end_point.excl,
            collators: vec![range_collator],
        });
    }
    Ok((ranges, false))
}

/// The error string of the skip-plan-cache reason (`%v` of `p.value`).
///
/// Go formats the `types.Datum` struct value (its fields, `Datum.String` has a pointer
/// receiver); here the datum's `String()` (D10.4 spec §5, known difference).
// glue: fmt "%v" of a types.Datum value
fn datum_v(d: &Datum) -> String {
    String::from_utf8_lossy(&d.string()).into_owned()
}

// go: convertPointInPlace L1
pub(crate) fn convert_point_in_place(
    sctx: &RangerContext,
    p: &mut Point,
    new_tp: &FieldType,
) -> Result<(), Error> {
    match p.value.kind() {
        KindMaxValue | KindMinNotNull => return Ok(()),
        _ => {}
    }
    let (mut casted, err) = p.value.convert_to(&sctx.type_ctx, new_tp);
    if let Err(err) = err {
        if sctx.in_prepared_plan_building {
            // skip plan cache in this case for safety.
            sctx.plan_cache_tracker.set_skip_plan_cache(&format!(
                "{} when converting {}",
                String::from_utf8_lossy(&err.error()),
                datum_v(&p.value)
            ));
        }
        if new_tp.get_type() == mysql::TypeYear && err_is(&err, &ErrWarnDataOutOfRange) {
            // see issue #20101: overflow when converting integer to year
        } else if new_tp.get_type() == mysql::TypeBit && err_is(&err, &ErrDataTooLong) {
            // see issue #19067: we should ignore the types.ErrDataTooLong when we convert value to TypeBit value
        } else if (new_tp.get_type() == mysql::TypeNewDecimal
            || mysql::is_integer_type(new_tp.get_type())
            || new_tp.get_type() == mysql::TypeFloat)
            && err_is(&err, &ErrOverflow)
        {
            // Ignore the types.ErrOverflow when we convert TypeNewDecimal/TypeTiny/TypeShort/TypeInt24/TypeLong/TypeLonglong/TypeFloat values.
            // A trimmed valid boundary point value would be returned then. Accordingly, the `excl` of the point
            // would be adjusted. Impossible ranges would be skipped by the `validInterval` call later.
            // tests in TestIndexRange/TestIndexRangeForDecimal
        } else if p.value.kind() == KindMysqlTime
            && new_tp.get_type() == mysql::TypeTimestamp
            && err_is(&err, &ErrWrongValue)
        {
            // See issue #28424: query failed after add index
            // Ignore conversion from Date[Time] to Timestamp since it must be either out of range or impossible date, which will not match a point select
        } else if new_tp.get_type() == mysql::TypeEnum && err_is(&err, &ErrTruncated) {
            // Ignore the types.ErrorTruncated when we convert TypeEnum values.
            // We should cover Enum upper overflow, and convert to the biggest value.
            if p.value.get_int64() > 0 {
                let elems = new_tp.get_elems().unwrap_or(&[]);
                let upper_enum = parse_enum_value(elems, elems.len() as u64)?;
                casted.set_mysql_enum(upper_enum, new_tp.get_collate().to_string());
            }
        } else if err_is(
            &err,
            &tidb_parser::gocharset::encoding_gbk::ErrInvalidCharacterString,
        ) {
            // The invalid string can be produced by changing datum's underlying bytes directly.
            // For example, newBuildFromPatternLike calculates the end point by adding 1 to bytes.
            // We need to skip these invalid strings.
            return Ok(());
        } else {
            return Err(err);
        }
    }
    let (val_cmp_casted, err) = p.value.compare(
        &sctx.type_ctx,
        &casted,
        collate::get_collator(new_tp.get_collate()),
    );
    err?;
    p.value = casted;
    if val_cmp_casted == 0 {
        return Ok(());
    }
    if p.start {
        if p.excl {
            if val_cmp_casted < 0 {
                // e.g. "a > 1.9" convert to "a >= 2".
                p.excl = false;
            }
        } else if val_cmp_casted > 0 {
            // e.g. "a >= 1.1 convert to "a > 1"
            p.excl = true;
        }
    } else if p.excl {
        if val_cmp_casted > 0 {
            // e.g. "a < 1.1" convert to "a <= 1"
            p.excl = false;
        }
    } else if val_cmp_casted < 0 {
        // e.g. "a <= 1.9" convert to "a < 2"
        p.excl = true;
    }
    Ok(())
}

// go: getRangesTotalDatumSize L1
fn get_ranges_total_datum_size(ranges: &[Range]) -> i64 {
    let mut sum = 0i64;
    for ran in ranges {
        for val in &ran.low_val {
            sum += val.mem_usage();
        }
        for val in &ran.high_val {
            sum += val.mem_usage();
        }
    }
    sum
}

// go: getPointsTotalDatumSize L1
fn get_points_total_datum_size(points: &[Point]) -> i64 {
    let mut sum = 0i64;
    for pt in points {
        sum += pt.value.mem_usage();
    }
    sum
}

/// estimateMemUsageForAppendPoints2Ranges estimates the memory usage of results of appending points to ranges.
// go: estimateMemUsageForAppendPoints2Ranges L1
fn estimate_mem_usage_for_append_points2_ranges(origin: &[Range], range_points: &[Point]) -> i64 {
    if origin.is_empty() || range_points.is_empty() {
        return 0;
    }
    let origin_datum_size = get_ranges_total_datum_size(origin);
    let point_datum_size = get_points_total_datum_size(range_points);
    let (len1, len2) = (origin.len() as i64, range_points.len() as i64 / 2);
    // (int64(len(origin[0].LowVal))+1)*16 is the size of Range.Collators.
    (EmptyRangeSize + (origin[0].low_val.len() as i64 + 1) * 16) * len1 * len2
        + origin_datum_size * len2
        + point_datum_size * len1
}

/// appendPoints2Ranges appends additional column ranges for multi-column index. The additional column ranges can only be
/// appended to point ranges. For example, we have an index (a, b), if the condition is (a > 1 and b = 2), then we can not
/// build a conjunctive ranges for this index.
/// rangeMaxSize is the max memory limit for ranges. O indicates no memory limit.
/// If the second return value is true, it means that the estimated memory usage of ranges after appending points exceeds
/// rangeMaxSize and the function rejects appending points to ranges.
// go: appendPoints2Ranges L1
pub(crate) fn append_points2_ranges(
    sctx: &RangerContext,
    origin: Ranges,
    range_points: Vec<Point>,
    new_tp: &FieldType,
    range_max_size: i64,
) -> Result<(Ranges, bool), Error> {
    let range_points = convert_points_in_place(sctx, range_points, new_tp, false, false)?;
    // Estimate whether rangeMaxSize will be exceeded first before appending points to ranges.
    if range_max_size > 0
        && estimate_mem_usage_for_append_points2_ranges(&origin, &range_points) > range_max_size
    {
        return Ok((origin, true));
    }
    let mut new_index_ranges: Ranges = Vec::new();
    for o_range in origin {
        if !o_range.is_point(sctx) {
            new_index_ranges.push(o_range);
        } else {
            let new_ranges = append_points2_index_range(&o_range, &range_points, new_tp);
            new_index_ranges.extend(new_ranges);
        }
    }
    Ok((new_index_ranges, false))
}

/// Go also returns an error, always nil.
// go: appendPoints2IndexRange L2 (the always-nil error is dropped)
fn append_points2_index_range(origin: &Range, range_points: &[Point], ft: &FieldType) -> Ranges {
    let range_count = range_points.len() / 2;
    let mut new_ranges: Ranges = Vec::with_capacity(range_count);
    let extra_collator = collate::get_collator(ft.get_collate());
    let mut i = 0usize;
    while i < range_points.len() {
        let (start_point, end_point) = (&range_points[i], &range_points[i + 1]);

        let mut low_val = Vec::with_capacity(origin.low_val.len() + 1);
        low_val.extend(origin.low_val.iter().cloned());
        low_val.push(start_point.value.clone());

        let mut high_val = Vec::with_capacity(origin.high_val.len() + 1);
        high_val.extend(origin.high_val.iter().cloned());
        high_val.push(end_point.value.clone());

        let mut collators = Vec::with_capacity(origin.collators.len() + 1);
        collators.extend(origin.collators.iter().copied());
        collators.push(extra_collator);

        new_ranges.push(Range {
            low_val,
            low_exclude: start_point.excl,
            high_val,
            high_exclude: end_point.excl,
            collators,
        });
        i += 2;
    }
    new_ranges
}

/// estimateMemUsageForAppendRanges2PointRanges estimates the memory usage of results of appending ranges to point ranges.
// go: estimateMemUsageForAppendRanges2PointRanges L1
fn estimate_mem_usage_for_append_ranges2_point_ranges(
    point_ranges: &[Range],
    ranges: &[Range],
) -> i64 {
    let (len1, len2) = (point_ranges.len() as i64, ranges.len() as i64);
    if len1 == 0 || len2 == 0 {
        return 0;
    }
    let collator_size =
        (point_ranges[0].low_val.len() as i64 + ranges[0].low_val.len() as i64) * 16;
    (EmptyRangeSize + collator_size) * len1 * len2
        + get_ranges_total_datum_size(point_ranges) * len2
        + get_ranges_total_datum_size(ranges) * len1
}

/// AppendRanges2PointRanges appends additional ranges to point ranges.
/// rangeMaxSize is the max memory limit for ranges. O indicates no memory limit.
/// If the second return value is true, it means that the estimated memory after appending additional ranges to point ranges
/// exceeds rangeMaxSize and the function rejects appending additional ranges to point ranges.
// go: AppendRanges2PointRanges L1
pub fn append_ranges2_point_ranges(
    point_ranges: Ranges,
    ranges: &[Range],
    range_max_size: i64,
) -> (Ranges, bool) {
    if ranges.is_empty() {
        return (point_ranges, false);
    }
    // Estimate whether rangeMaxSize will be exceeded first before appending ranges to point ranges.
    if range_max_size > 0
        && estimate_mem_usage_for_append_ranges2_point_ranges(&point_ranges, ranges)
            > range_max_size
    {
        return (point_ranges, true);
    }
    let range_count = point_ranges.len() * ranges.len();
    let mut new_ranges: Ranges = Vec::with_capacity(range_count);
    for point_range in &point_ranges {
        for r in ranges {
            let mut low_val = Vec::with_capacity(point_range.low_val.len() + r.low_val.len());
            low_val.extend(point_range.low_val.iter().cloned());
            low_val.extend(r.low_val.iter().cloned());

            let mut high_val = Vec::with_capacity(point_range.high_val.len() + r.high_val.len());
            high_val.extend(point_range.high_val.iter().cloned());
            high_val.extend(r.high_val.iter().cloned());

            let mut collators = Vec::with_capacity(point_range.collators.len() + r.collators.len());
            collators.extend(point_range.collators.iter().copied());
            collators.extend(r.collators.iter().copied());

            new_ranges.push(Range {
                low_val,
                low_exclude: r.low_exclude,
                high_val,
                high_exclude: r.high_exclude,
                collators,
            });
        }
    }
    (new_ranges, false)
}

/// points2TableRanges build ranges for table scan from range points.
/// It will remove the nil and convert MinNotNull and MaxValue to MinInt64 or MinUint64 and MaxInt64 or MaxUint64.
/// rangeMaxSize is the max memory limit for ranges. O indicates no memory limit.
/// If the second return value is true, it means that the estimated memory usage of ranges exceeds rangeMaxSize and it falls back to full range.
// go: points2TableRanges L1
fn points2_table_ranges(
    sctx: &RangerContext,
    range_points: Vec<Point>,
    new_tp: &FieldType,
    range_max_size: i64,
) -> Result<(Ranges, bool), Error> {
    let range_points = convert_points_in_place(sctx, range_points, new_tp, true, true)?;
    if range_max_size > 0 && estimate_mem_usage_for_points2_ranges(&range_points) > range_max_size {
        return Ok((
            full_int_range(mysql::has_unsigned_flag(new_tp.get_flag())),
            true,
        ));
    }
    let mut ranges: Ranges = Vec::with_capacity(range_points.len() / 2);
    let mut it = range_points.into_iter();
    while let (Some(start_point), Some(end_point)) = (it.next(), it.next()) {
        ranges.push(Range {
            low_val: vec![start_point.value],
            low_exclude: start_point.excl,
            high_val: vec![end_point.value],
            high_exclude: end_point.excl,
            collators: vec![collate::get_collator(new_tp.get_collate())],
        });
    }
    Ok((ranges, false))
}

/// The result of `BuildTableRange` / `BuildColumnRange`: Go's `(Ranges, accessConds,
/// remainedConds, error)`.
pub type ColumnRangeResult = (Ranges, Vec<ExprRef>, Vec<ExprRef>);

/// buildColumnRange builds range from CNF conditions.
/// rangeMaxSize is the max memory limit for ranges. O indicates no memory limit.
/// The second return value is the conditions used to build ranges and the third return value is the remained conditions.
// go: buildColumnRange L1
fn build_column_range_impl(
    access_conditions: Vec<ExprRef>,
    sctx: &RangerContext,
    tp: &FieldType,
    table_range: bool,
    col_len: isize,
    range_max_size: i64,
) -> Result<ColumnRangeResult, Error> {
    let mut rb = Builder::new(sctx);
    let new_tp = new_field_type_for_ranger(tp);
    let mut range_points = get_full_range();
    for cond in &access_conditions {
        let collator = collate::get_collator(CollationBin);
        let b = rb.build(cond, &new_tp, col_len, true);
        range_points = rb.intersection(range_points, b, collator);
        if let Some(err) = rb.err.take() {
            return Err(err);
        }
    }
    let new_tp = convert_string_ft_to_binary_collate(&new_tp);
    let (mut ranges, range_fallback) = if table_range {
        points2_table_ranges(sctx, range_points, &new_tp, range_max_size)?
    } else {
        points2_ranges(sctx, range_points, &new_tp, range_max_size)?
    };
    if range_fallback {
        sctx.range_fallback_handler
            .record_range_fallback(range_max_size);
        return Ok((ranges, Vec::new(), access_conditions));
    }
    if col_len != UnspecifiedLength {
        ranges = union_ranges(sctx, ranges, true)?;
    }
    Ok((ranges, access_conditions, Vec::new()))
}

/// BuildTableRange builds range of PK column for PhysicalTableScan.
/// rangeMaxSize is the max memory limit for ranges. O indicates no memory limit. If you ask that all conds must be used
/// for building ranges, set rangeMemQuota to 0 to avoid range fallback.
/// The second return value is the conditions used to build ranges and the third return value is the remained conditions.
/// If you use the function to build ranges for some access path, you need to update the path's access conditions and filter
/// conditions by the second and third return values respectively.
// go: BuildTableRange L1
pub fn build_table_range(
    access_conditions: Vec<ExprRef>,
    sctx: &RangerContext,
    tp: &FieldType,
    range_max_size: i64,
) -> Result<ColumnRangeResult, Error> {
    build_column_range_impl(
        access_conditions,
        sctx,
        tp,
        true,
        UnspecifiedLength,
        range_max_size,
    )
}

/// BuildColumnRange builds range from access conditions for general columns.
/// rangeMaxSize is the max memory limit for ranges. O indicates no memory limit. If you ask that all conds must be used
/// for building ranges, set rangeMemQuota to 0 to avoid range fallback.
/// The second return value is the conditions used to build ranges and the third return value is the remained conditions.
/// If you use the function to build ranges for some access path, you need to update the path's access conditions and filter
/// conditions by the second and third return values respectively.
// go: BuildColumnRange L1
pub fn build_column_range(
    conds: Vec<ExprRef>,
    sctx: &RangerContext,
    tp: &FieldType,
    col_len: isize,
    range_mem_quota: i64,
) -> Result<ColumnRangeResult, Error> {
    if conds.is_empty() {
        return Ok((full_range(), Vec::new(), Vec::new()));
    }
    build_column_range_impl(conds, sctx, tp, false, col_len, range_mem_quota)
}

impl RangeDetacher<'_> {
    // go: rangeDetacher.buildRangeOnColsByCNFCond L1
    pub(crate) fn build_range_on_cols_by_cnf_cond(
        &self,
        new_tp: &[FieldType],
        eq_and_in_count: usize,
        access_conds: Vec<ExprRef>,
    ) -> Result<ColumnRangeResult, Error> {
        let mut rb = Builder::new(self.sctx);
        let mut ranges: Ranges = Vec::new();
        for i in 0..eq_and_in_count {
            // Build ranges for equal or in access conditions.
            let point = rb.build(
                &access_conds[i],
                &new_tp[i],
                self.lengths[i],
                self.convert_to_sort_key,
            );
            if let Some(err) = rb.err.take() {
                return Err(err);
            }
            let tmp_new_tp = if self.convert_to_sort_key {
                convert_string_ft_to_binary_collate(&new_tp[i])
            } else {
                new_tp[i].clone()
            };
            let range_fallback;
            if i == 0 {
                (ranges, range_fallback) =
                    points2_ranges(self.sctx, point, &tmp_new_tp, self.range_max_size)?;
            } else {
                (ranges, range_fallback) = append_points2_ranges(
                    self.sctx,
                    ranges,
                    point,
                    &tmp_new_tp,
                    self.range_max_size,
                )?;
            }
            if range_fallback {
                self.sctx
                    .range_fallback_handler
                    .record_range_fallback(self.range_max_size);
                let remained = access_conds[i..].to_vec();
                let mut access = access_conds;
                access.truncate(i);
                return Ok((ranges, access, remained));
            }
        }
        let mut range_points = get_full_range();
        // Build rangePoints for non-equal access conditions.
        for cond in access_conds.iter().skip(eq_and_in_count) {
            let mut collator = collate::get_collator(new_tp[eq_and_in_count].get_collate());
            if self.convert_to_sort_key {
                collator = collate::get_collator(CollationBin);
            }
            let b = rb.build(
                cond,
                &new_tp[eq_and_in_count],
                self.lengths[eq_and_in_count],
                self.convert_to_sort_key,
            );
            range_points = rb.intersection(range_points, b, collator);
            if let Some(err) = rb.err.take() {
                return Err(err);
            }
        }
        let mut tmp_new_tp: Option<FieldType> = None;
        if eq_and_in_count == 0 || eq_and_in_count < access_conds.len() {
            if self.convert_to_sort_key {
                tmp_new_tp = Some(convert_string_ft_to_binary_collate(
                    &new_tp[eq_and_in_count],
                ));
            } else {
                tmp_new_tp = Some(new_tp[eq_and_in_count].clone());
            }
        }
        let mut range_fallback = false;
        if eq_and_in_count == 0 {
            (ranges, range_fallback) = points2_ranges(
                self.sctx,
                range_points,
                tmp_new_tp.as_ref().expect("set above"),
                self.range_max_size,
            )?;
        } else if eq_and_in_count < access_conds.len() {
            (ranges, range_fallback) = append_points2_ranges(
                self.sctx,
                ranges,
                range_points,
                tmp_new_tp.as_ref().expect("set above"),
                self.range_max_size,
            )?;
        }
        if range_fallback {
            self.sctx
                .range_fallback_handler
                .record_range_fallback(self.range_max_size);
            let remained = access_conds[eq_and_in_count..].to_vec();
            let mut access = access_conds;
            access.truncate(eq_and_in_count);
            return Ok((ranges, access, remained));
        }
        Ok((ranges, access_conds, Vec::new()))
    }

    /// buildCNFIndexRange builds the range for index where the top layer is CNF.
    // go: rangeDetacher.buildCNFIndexRange L1
    pub(crate) fn build_cnf_index_range(
        &self,
        new_tp: &[FieldType],
        eq_and_in_count: usize,
        access_conds: Vec<ExprRef>,
    ) -> Result<ColumnRangeResult, Error> {
        let (mut ranges, new_access_conds, remained_conds) =
            self.build_range_on_cols_by_cnf_cond(new_tp, eq_and_in_count, access_conds)?;

        // Take prefix index into consideration.
        if has_prefix(self.lengths) {
            ranges = union_ranges(self.sctx, ranges, self.merge_consecutive)?;
        }

        Ok((ranges, new_access_conds, remained_conds))
    }
}

// go: convertStringFTToBinaryCollate L1
pub(crate) fn convert_string_ft_to_binary_collate(ft: &FieldType) -> FieldType {
    if ft.eval_type() != ETString
        || ft.get_type() == mysql::TypeEnum
        || ft.get_type() == mysql::TypeSet
    {
        return ft.clone();
    }
    let mut new_tp = ft.clone();
    new_tp.set_charset(CharsetBin);
    new_tp.set_collate(CollationBin);
    new_tp
}

/// `sortRange` of UnionRanges.
struct SortRange {
    original_value: Range,
    encoded_start: Vec<u8>,
    encoded_end: Vec<u8>,
}

/// UnionRanges sorts `ranges`, union adjacent ones if possible.
/// For two intervals [a, b], [c, d], we have guaranteed that a <= c. If b >= c. Then two intervals are overlapped.
/// And this two can be merged as [a, max(b, d)].
/// Otherwise they aren't overlapped.
///
/// Go returns `nil` for no ranges; an empty `Vec` here.
// go: UnionRanges L1
pub fn union_ranges(
    sctx: &RangerContext,
    ranges: Ranges,
    merge_consecutive: bool,
) -> Result<Ranges, Error> {
    if ranges.is_empty() {
        return Ok(Vec::new());
    }
    let mut objects: Vec<SortRange> = Vec::with_capacity(ranges.len());
    for ran in ranges {
        let mut left = encode_key_handled(sctx, &ran.low_val)?;
        if ran.low_exclude {
            left = Key(left).prefix_next().0;
        }
        let mut right = encode_key_handled(sctx, &ran.high_val)?;
        if !ran.high_exclude {
            right = Key(right).prefix_next().0;
        }
        objects.push(SortRange {
            original_value: ran,
            encoded_start: left,
            encoded_end: right,
        });
    }
    sort(&mut objects, |i, j| i.encoded_start < j.encoded_start);
    let mut out: Ranges = Vec::new();
    let mut it = objects.into_iter();
    let mut last_range = it.next().expect("non-empty");
    for obj in it {
        if (merge_consecutive && last_range.encoded_end >= obj.encoded_start)
            || (!merge_consecutive && last_range.encoded_end > obj.encoded_start)
        {
            if last_range.encoded_end < obj.encoded_end {
                last_range.encoded_end = obj.encoded_end;
                last_range.original_value.high_val = obj.original_value.high_val;
                last_range.original_value.high_exclude = obj.original_value.high_exclude;
            }
        } else {
            out.push(last_range.original_value);
            last_range = obj;
        }
    }
    out.push(last_range.original_value);
    Ok(out)
}

// go: hasPrefix L1
pub(crate) fn has_prefix(lengths: &[isize]) -> bool {
    lengths.iter().any(|&l| l != UnspecifiedLength)
}

/// cutPrefixForPoints cuts the prefix of points according to the prefix length of the prefix index.
/// It may modify the point.value and point.excl. The modification is in-place.
/// This function doesn't require the start and end points to be paired in the input.
// go: cutPrefixForPoints L1
pub(crate) fn cut_prefix_for_points(points: &mut [Point], length: isize, tp: &FieldType) {
    if length == UnspecifiedLength {
        return;
    }
    for p in points.iter_mut() {
        let cut = cut_datum_by_prefix_len(&mut p.value, length, tp);
        // In two cases, we need to convert the exclusive point to an inclusive point.
        // case 1: we actually cut the value to accommodate the prefix index.
        if cut
            // case 2: the value is already equal to the prefix index.
            // For example, col_varchar > 'xx' should be converted to range [xx, +inf) when the prefix index length of
            // `col_varchar` is 2. Otherwise, we would miss values like 'xxx' if we execute (xx, +inf) index range scan.
            || (p.start && reach_prefix_len(&p.value, length, tp))
        {
            p.excl = false;
        }
    }
}

/// CutDatumByPrefixLen cuts the datum according to the prefix length.
/// If it's binary or ascii encoded, we will cut it by bytes rather than characters.
// go: CutDatumByPrefixLen L1
pub fn cut_datum_by_prefix_len(v: &mut Datum, length: isize, tp: &FieldType) -> bool {
    if (v.kind() == KindString || v.kind() == KindBytes) && length != UnspecifiedLength {
        let col_charset = tp.get_charset();
        let col_value = v.get_bytes().to_vec();
        let length = length as usize;
        if col_charset == CharsetBin || col_charset == CharsetASCII {
            if col_value.len() > length {
                // truncate value and limit its length
                if v.kind() == KindBytes {
                    v.set_bytes(col_value[..length].to_vec());
                } else {
                    v.set_string(col_value[..length].to_vec(), tp.get_collate().to_string());
                }
                return true;
            }
        } else if rune_count(&col_value) > length {
            let truncate_str = runes_prefix(&col_value, length);
            // truncate value and limit its length
            v.set_string(truncate_str, tp.get_collate().to_string());
            return true;
        }
    }
    false
}

/// ReachPrefixLen checks whether the length of v is equal to the prefix length.
// go: ReachPrefixLen L1
pub fn reach_prefix_len(v: &Datum, length: isize, tp: &FieldType) -> bool {
    if (v.kind() == KindString || v.kind() == KindBytes) && length != UnspecifiedLength {
        let col_charset = tp.get_charset();
        let col_value = v.get_bytes();
        if col_charset == CharsetBin || col_charset == CharsetASCII {
            return col_value.len() == length as usize;
        }
        return rune_count(col_value) == length as usize;
    }
    false
}

/// `utf8.RuneCount`: invalid bytes count as one rune each.
// go-std: unicode/utf8.RuneCount L1
fn rune_count(b: &[u8]) -> usize {
    go_runes(b).len()
}

/// `string(bytes.Runes(b)[:n])`: invalid bytes decode to U+FFFD and are written back as
/// its 3-byte encoding.
// go-std: bytes.Runes L1
fn runes_prefix(b: &[u8], n: usize) -> Vec<u8> {
    let mut out = String::new();
    for r in go_runes(b).into_iter().take(n) {
        out.push(r);
    }
    out.into_bytes()
}

/// Go's rune decoding of a byte string (`for range` / `utf8.DecodeRune`): each invalid
/// byte is one `utf8.RuneError`.
// go-std: unicode/utf8.DecodeRune L1
fn go_runes(mut b: &[u8]) -> Vec<char> {
    let mut out = Vec::new();
    while !b.is_empty() {
        match std::str::from_utf8(b) {
            Ok(s) => {
                out.extend(s.chars());
                break;
            }
            Err(e) => {
                let valid = e.valid_up_to();
                out.extend(std::str::from_utf8(&b[..valid]).expect("valid").chars());
                out.push(char::REPLACEMENT_CHARACTER);
                // Go consumes one byte per invalid rune.
                b = &b[valid + 1..];
            }
        }
    }
    out
}

/// In util/ranger, for each datum that is used in the Range, we will convert data type for them.
/// But we cannot use the FieldType of column directly. e.g. the column a is int32 and we have a > 1111111111111111111.
/// Obviously the constant is bigger than MaxInt32, so we will get overflow error if we use the FieldType of column a.
/// In util/ranger here, we usually use "newTp" to emphasize its difference from the original FieldType of the column.
///
/// Go returns the same pointer in the default case; a copy here (never changed in place).
// go: newFieldType L1
pub(crate) fn new_field_type_for_ranger(tp: &FieldType) -> FieldType {
    match tp.get_type() {
        // To avoid overflow error.
        mysql::TypeTiny
        | mysql::TypeShort
        | mysql::TypeInt24
        | mysql::TypeLong
        | mysql::TypeLonglong => {
            let mut new_tp = new_field_type(mysql::TypeLonglong);
            new_tp.set_flag(tp.get_flag());
            new_tp.set_charset(tp.get_charset());
            new_tp
        }
        // To avoid data truncate error.
        mysql::TypeFloat
        | mysql::TypeDouble
        | mysql::TypeBlob
        | mysql::TypeTinyBlob
        | mysql::TypeMediumBlob
        | mysql::TypeLongBlob
        | mysql::TypeString
        | mysql::TypeVarchar
        | mysql::TypeVarString => {
            let mut new_tp =
                new_field_type_with_collation(tp.get_type(), tp.get_collate(), UnspecifiedLength);
            new_tp.set_charset(tp.get_charset());
            new_tp
        }
        _ => tp.clone(),
    }
}

/// `expression.NewFunctionInternal` for the conditions ranger rebuilds. A function whose
/// class is not ported yet (`in`, D08.9) is the boundary error of that block; any other
/// build error is logged and gives `None`, as Go's `NewFunctionInternal`.
// glue: NewFunctionInternal + boundary of the unported function classes (constitution §4.3)
pub(crate) fn new_function_internal_or_blocked(
    ctx: &dyn BuildContext,
    func_name: &str,
    ret_type: FieldType,
    args: Vec<ExprRef>,
) -> Result<Option<ExprRef>, Error> {
    if tidb_expression::builtin_registry::function_class(func_name).is_none() {
        // glue: boundary of NewFunctionInternal(in) (another block), blocked-by:D08.9 (the IN function class)
        return Err(tidb_base::blocked::blocked(
            "D08.9",
            &format!("expression.NewFunctionInternal({func_name})"),
        ));
    }
    match new_function(ctx, func_name, ret_type, args) {
        Ok(e) => Ok(Some(e)),
        Err(err) => {
            tidb_base::terror::log(Some(&err));
            Ok(None)
        }
    }
}

/// points2EqOrInCond constructs a 'EQUAL' or 'IN' scalar function based on the
/// 'points'. `col` is the target column to construct the Equal or In condition.
/// NOTE:
/// 1. 'points' should not be empty.
// go: points2EqOrInCond L2 (Result for the D08.9 boundary of the IN builtin)
pub(crate) fn points2_eq_or_in_cond(
    ctx: &dyn BuildContext,
    points: &[Point],
    col: &Arc<Column>,
) -> Result<Option<ExprRef>, Error> {
    // len(points) cannot be 0 here, since we impose early termination in ExtractEqAndInCondition
    // Constant and Column args should have same RetType, simply get from first arg
    let ret_type: FieldType = (*col.ret_type).clone();
    let col_expr: ExprRef = Expr::Column(Column::clone(col)).into_ref();
    let mut args: Vec<ExprRef> = Vec::with_capacity(points.len() / 2);
    args.push(Arc::clone(&col_expr));
    let mut or_args: Vec<ExprRef> = Vec::with_capacity(2);
    let mut i = 0usize;
    while i < points.len() {
        if points[i].value.is_null() {
            if let Some(e) = new_function_internal_or_blocked(
                ctx,
                ast::IsNull,
                ret_type.clone(),
                vec![Arc::clone(&col_expr)],
            )? {
                or_args.push(e);
            }
        } else {
            let value = Constant::new(points[i].value.clone(), ret_type.clone());
            args.push(Expr::Constant(value).into_ref());
        }
        i += 2;
    }
    let mut result: Option<ExprRef> = None;
    if args.len() > 1 {
        let mut func_name = ast::EQ;
        if args.len() > 2 {
            func_name = ast::In;
        }
        result = new_function_internal_or_blocked(ctx, func_name, ret_type.clone(), args)?;
    }
    if or_args.is_empty() {
        return Ok(result);
    }
    if let Some(result) = result {
        or_args.push(result);
    }
    if or_args.len() == 1 {
        return Ok(or_args.pop());
    }
    new_function_internal_or_blocked(ctx, ast::LogicOr, ret_type, or_args)
}

/// RangesToString print a list of Ranges into a string which can appear in an SQL as a condition.
///
/// Go returns `(string, error)` with a meaningful string on some errors ("false"); here
/// `Err` carries the error and the string is dropped (M-ERRVAL: the callers return on the
/// error). The string is bytes (M-BYTES: string values of the ranges are written as they are).
// go: RangesToString L1 [M-ERRVAL, M-BYTES]
#[allow(clippy::needless_range_loop)] // Go's index loop over the parallel slices of a range
pub fn ranges_to_string(
    sc: &StatementContext,
    rans: &[Range],
    col_names: &[String],
) -> Result<Vec<u8>, Error> {
    for ran in rans {
        if ran.low_val.len() != ran.high_val.len() {
            return Err(Error::plain("range length mismatch"));
        }
    }
    let mut buffer: Vec<u8> = Vec::new();
    for (i, ran) in rans.iter().enumerate() {
        buffer.push(b'(');
        for j in 0..ran.low_val.len() {
            buffer.push(b'(');

            // The `Exclude` information is only useful for the last columns.
            // If it's not the last column, it should always be false, which means it's inclusive.
            let mut low_exclude = false;
            if ran.low_exclude && j == ran.low_val.len() - 1 {
                low_exclude = true;
            }
            let mut high_exclude = false;
            if ran.high_exclude && j == ran.low_val.len() - 1 {
                high_exclude = true;
            }

            // sanity check: only last column of the `Range` can be an interval
            if j < ran.low_val.len() - 1 {
                let (cmp, err) =
                    ran.low_val[j].compare(&sc.type_ctx(), &ran.high_val[j], ran.collators[j]);
                if let Err(err) = err {
                    let mut msg = b"comparing values error: ".to_vec();
                    msg.extend(err.error());
                    return Err(Error::plain(msg));
                }
                if cmp != 0 {
                    return Err(Error::plain("unexpected form of range"));
                }
            }
            let str = range_single_col_to_string(
                sc,
                &ran.low_val[j],
                &ran.high_val[j],
                low_exclude,
                high_exclude,
                &col_names[j],
                ran.collators[j],
            )?;
            buffer.extend(str);
            buffer.push(b')');
            if j < ran.low_val.len() - 1 {
                // Conditions on different columns of a range are implicitly connected with AND.
                buffer.extend_from_slice(b" and ");
            }
        }
        buffer.push(b')');
        if i < rans.len() - 1 {
            // Conditions of different ranges are implicitly connected with OR.
            buffer.extend_from_slice(b" or ");
        }
    }
    let result = buffer;

    // Simplify some useless conditions.
    // regexp `^\(*true\)*$`.
    let start = result.iter().take_while(|&&c| c == b'(').count();
    if let Some(rest) = result[start..].strip_prefix(b"true".as_slice())
        && rest.iter().all(|&c| c == b')')
    {
        return Ok(b"true".to_vec());
    }
    Ok(result)
}

/// `driver.ValueExpr{Datum: v}.Restore(format.NewRestoreCtx(format.DefaultRestoreFlags, &buf))`:
/// the value expression has a zero `Type` (no charset, no flags), so strings get no
/// charset introducer and integers are never booleans. `ValueExpr` is D02.2 (the Rust
/// parser's `ValueExpr` carries its own literal type, not a `types.Datum`).
// glue: parser_driver ValueExpr.Restore with a zero Type and DefaultRestoreFlags (owned by D02.2)
fn restore_value(buf: &mut Vec<u8>, d: &Datum) -> Result<(), Error> {
    // glue: format.RestoreCtx.WriteString with RestoreStringSingleQuotes (parser format, D02.2)
    fn write_string(buf: &mut Vec<u8>, s: &[u8]) {
        buf.push(b'\'');
        for &c in s {
            if c == b'\'' {
                buf.push(b'\'');
            }
            buf.push(c);
        }
        buf.push(b'\'');
    }
    match d.kind() {
        KindNull => buf.extend_from_slice(b"NULL"),
        KindInt64 => buf.extend(d.get_int64().to_string().into_bytes()),
        KindUint64 => buf.extend(d.get_uint64().to_string().into_bytes()),
        KindFloat32 => buf.extend(format_float(d.get_float64(), b'e', -1, 32).into_bytes()),
        KindFloat64 => buf.extend(format_float(d.get_float64(), b'e', -1, 64).into_bytes()),
        KindString => {
            // Replace '\' to '\\' regardless of sql_mode "NO_BACKSLASH_ESCAPES", which is the same as MySQL.
            let mut s = Vec::with_capacity(d.get_string().len());
            for &c in d.get_string() {
                if c == b'\\' {
                    s.push(b'\\');
                }
                s.push(c);
            }
            write_string(buf, &s);
        }
        KindBytes => write_string(buf, d.get_string()),
        KindMysqlDecimal => buf.extend(d.get_mysql_decimal().string().into_bytes()),
        KindBinaryLiteral => buf.extend(
            d.get_binary_literal()
                .to_bit_literal_string(true)
                .into_bytes(),
        ),
        KindMysqlDuration => {
            buf.push(b'\'');
            buf.extend(d.get_mysql_duration().string());
            buf.push(b'\'');
        }
        KindMysqlTime => {
            buf.push(b'\'');
            buf.extend(d.get_mysql_time().string());
            buf.push(b'\'');
        }
        KindMysqlEnum | KindMysqlBit | KindMysqlSet | KindInterface | KindMinNotNull
        | KindMaxValue | KindRaw | KindMysqlJSON | KindVectorFloat32 => {
            // TODO implement Restore function
            return Err(Error::plain("Not implemented"));
        }
        _ => return Err(Error::plain("can't format to string")),
    }
    Ok(())
}

/// RangeSingleColToString prints a single column of a Range into a string which can appear in an SQL as a condition.
///
/// Go returns `"false"` with its errors; `Err` here (M-ERRVAL).
// go: RangeSingleColToString L1 [M-ERRVAL, M-BYTES]
pub fn range_single_col_to_string(
    sc: &StatementContext,
    low_val: &Datum,
    high_val: &Datum,
    low_exclude: bool,
    high_exclude: bool,
    col_name: &str,
    collator: &dyn Collator,
) -> Result<Vec<u8>, Error> {
    // case 1: low and high are both special values(null, min not null, max value)
    let low_kind = low_val.kind();
    let high_kind = high_val.kind();
    if (low_kind == KindNull || low_kind == KindMinNotNull || low_kind == KindMaxValue)
        && (high_kind == KindNull || high_kind == KindMinNotNull || high_kind == KindMaxValue)
    {
        if low_kind == KindNull && high_kind == KindNull && !low_exclude && !high_exclude {
            return Ok(format!("{col_name} is null").into_bytes());
        }
        if low_kind == KindNull && high_kind == KindMaxValue && !low_exclude {
            return Ok(b"true".to_vec());
        }
        if low_kind == KindMinNotNull && high_kind == KindMaxValue {
            return Ok(format!("{col_name} is not null").into_bytes());
        }
        return Ok(b"false".to_vec());
    }

    let mut buf: Vec<u8> = Vec::new();

    // case 2: low value and high value are the same, and low value and high value are both inclusive.
    let (cmp, err) = low_val.compare(&sc.type_ctx(), high_val, collator);
    err?;
    if cmp == 0 && !low_exclude && !high_exclude && !low_val.is_null() {
        buf.extend_from_slice(col_name.as_bytes());
        buf.extend_from_slice(b" = ");
        restore_value(&mut buf, low_val)?;
        return Ok(buf);
    }

    // case 3: it's an interval.
    let mut use_or = false;
    let mut no_lower_part = false;

    // Handle the low value part.
    if low_kind == KindNull {
        buf.extend(format!("{col_name} is null").into_bytes());
        use_or = true;
    } else if low_kind == KindMinNotNull {
        no_lower_part = true;
    } else {
        buf.extend_from_slice(col_name.as_bytes());
        if low_exclude {
            buf.extend_from_slice(b" > ");
        } else {
            buf.extend_from_slice(b" >= ");
        }
        restore_value(&mut buf, low_val)?;
    }

    if !no_lower_part {
        if use_or {
            buf.extend_from_slice(b" or ");
        } else {
            buf.extend_from_slice(b" and ");
        }
    }

    // Handle the high value part
    if high_kind == KindMaxValue {
        buf.extend_from_slice(b"true");
    } else {
        buf.extend_from_slice(col_name.as_bytes());
        if high_exclude {
            buf.extend_from_slice(b" < ");
        } else {
            buf.extend_from_slice(b" <= ");
        }
        restore_value(&mut buf, high_val)?;
    }

    Ok(buf)
}
