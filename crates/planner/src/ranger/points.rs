//! go-file: pkg/util/ranger/points.go
//!
//! The range points: building them from one condition on one column (`builder`), and
//! their union / intersection. `FullIntRange`, `FullRange`, `FullNotNullRange` are D00.1
//! (`tidb_sessionctx::ranger::types`).
//!
//! Mapping (D10.4 spec §3):
//! - Go's `[]*point` is `Vec<Point>`: no Go caller keeps two slots pointing at one point
//!   while it is changed in place (`convertPointsInPlace`, `cutPrefixForPoints`,
//!   `excludeToIncludeForIntPoint`), so values behave the same.
//! - The `builder.err` field is kept: Go's functions return points and leave the error on
//!   the builder; callers check it after each call.
//! - `slices.SortFunc` is Go's pdqsort ([`tidb_base::gostd::sort::sort`]): the order of
//!   points that compare equal (e.g. 'a' and 'A' under a `_ci` collation) and the error of
//!   the last comparison are Go's.

// Datum kinds, AST function names and type codes keep their Go names (M-GONAME).
#![allow(non_upper_case_globals)]

use tidb_base::Error;
use tidb_base::dbterror::plannererrors::ErrUnsupportedType;
use tidb_base::errors::RedactLogDisable;
use tidb_base::gofmt::{Arg, sprintf};
use tidb_base::gostd::sort::sort;
use tidb_base::mysql;
use tidb_chunk::Row;
use tidb_expression::scalar_function::ast_func as ast;
use tidb_expression::{EvalState, Expr, ScalarFunction};
use tidb_parser::gocharset::CollationBin;
use tidb_parser::types::ETString;
use tidb_parser::types::{FieldType, UnspecifiedLength};
use tidb_sessionctx::ranger::context::RangerContext;
use tidb_types::collate::{self, Collator};
use tidb_types::context::Context as TypeContext;
use tidb_types::datum::*;
use tidb_types::enum_type::{Enum, parse_enum_name};
use tidb_types::mydecimal::MyDecimal;

use super::ranger::{convert_point_in_place, cut_prefix_for_points};

/// RangeType is alias for int.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RangeType {
    IntRangeType,
    ColumnRangeType,
    IndexRangeType,
}

/// Point is the end point of range interval.
#[derive(Clone, Debug, Default)]
pub struct Point {
    pub value: Datum,
    /// exclude
    pub excl: bool,
    pub start: bool,
}

impl Point {
    // go: point.String L1
    pub fn string(&self) -> Vec<u8> {
        let mut val = self.value.get_value().arg();
        if self.value.kind() == KindMinNotNull {
            val = Arg::from("-inf");
        } else if self.value.kind() == KindMaxValue {
            val = Arg::from("+inf");
        }
        if self.start {
            let mut symbol = "[";
            if self.excl {
                symbol = "(";
            }
            return sprintf("%s%v", &[Arg::from(symbol), val]);
        }
        let mut symbol = "]";
        if self.excl {
            symbol = ")";
        }
        sprintf("%v%s", &[val, Arg::from(symbol)])
    }
}

/// `point{start: true}` etc.: a point with a zero (NULL) value.
// glue: Go struct literal point{...}
fn pt(value: Datum, excl: bool, start: bool) -> Point {
    Point { value, excl, start }
}

/// `types.Datum` with `SetInt64(0)` (the zero points of IS TRUE / IS FALSE).
// glue: Go `p.value.SetInt64(0)` on a zero Datum
fn zero_int() -> Datum {
    let mut d = Datum::default();
    d.set_int64(0);
    d
}

// go: rangePointCmp L1
pub(crate) fn range_point_cmp(
    tc: &TypeContext,
    a: &Point,
    b: &Point,
    collator: &dyn Collator,
) -> (isize, Result<(), Error>) {
    if a.value.kind() == KindMysqlEnum && b.value.kind() == KindMysqlEnum {
        return range_point_enum_cmp(a, b);
    }
    let (cmp, err) = a.value.compare(tc, &b.value, collator);
    if cmp != 0 {
        return (cmp, Ok(()));
    }
    (range_point_equal_value_cmp(a, b), err)
}

// go: rangePointEnumCmp L1
fn range_point_enum_cmp(a: &Point, b: &Point) -> (isize, Result<(), Error>) {
    let cmp = a.value.get_int64().cmp(&b.value.get_int64()) as isize;
    if cmp != 0 {
        return (cmp, Ok(()));
    }
    (range_point_equal_value_cmp(a, b), Ok(()))
}

// go: rangePointEqualValueCmp L1
fn range_point_equal_value_cmp(a: &Point, b: &Point) -> isize {
    let result = if a.start && b.start {
        !a.excl && b.excl
    } else if a.start {
        !a.excl && !b.excl
    } else if b.start {
        a.excl || b.excl
    } else {
        a.excl && !b.excl
    };
    if result {
        return -1;
    }
    0
}

// go: convertPointsToSortKeyInPlace L1
pub(crate) fn convert_points_to_sort_key_in_place(
    sctx: &RangerContext,
    points: &mut [Point],
    new_tp: &FieldType,
) -> Result<(), Error> {
    // Only handle normal string type here.
    // Currently, set won't be pushed down and it shouldn't reach here in theory.
    // For enum, we have separate logic for it, like handleEnumFromBinOp(). For now, it only supports point range,
    // intervals are not supported. So we also don't need to handle enum here.
    if new_tp.eval_type() != ETString
        || new_tp.get_type() == mysql::TypeEnum
        || new_tp.get_type() == mysql::TypeSet
    {
        return Ok(());
    }
    for p in points.iter_mut() {
        convert_point_to_sort_key_in_place(sctx, p, new_tp, true)?;
    }
    Ok(())
}

// go: convertPointToSortKeyInPlace L1
pub(crate) fn convert_point_to_sort_key_in_place(
    sctx: &RangerContext,
    p: &mut Point,
    new_tp: &FieldType,
    trim_trailing_space: bool,
) -> Result<(), Error> {
    convert_point_in_place(sctx, p, new_tp)?;
    if p.value.kind() != KindString
        || new_tp.get_collate() == CollationBin
        || !collate::new_collation_enabled()
    {
        return Ok(());
    }
    let sort_key = p.value.get_bytes();
    let sort_key = if !trim_trailing_space {
        collate::get_collator(new_tp.get_collate()).key_without_trim_right_space(sort_key)
    } else {
        collate::get_collator(new_tp.get_collate()).key(sort_key)
    };

    p.value = new_bytes_datum(sort_key);
    Ok(())
}

// If use []point, fullRange will be copied when used.
// So for keep this behaver, getFullRange function is introduced.
// go: getFullRange L1
pub(crate) fn get_full_range() -> Vec<Point> {
    vec![
        pt(Datum::default(), false, true),
        pt(max_value_datum(), false, false),
    ]
}

// go: getNotNullFullRange L1
pub(crate) fn get_not_null_full_range() -> Vec<Point> {
    vec![
        pt(min_not_null_datum(), false, true),
        pt(max_value_datum(), false, false),
    ]
}

/// NullRange is [null, null] for Range.
// go: NullRange L1
pub fn null_range() -> tidb_sessionctx::ranger::Ranges {
    vec![tidb_sessionctx::ranger::Range {
        low_val: vec![Datum::default()],
        high_val: vec![Datum::default()],
        collators: collate::get_binary_collator_slice(1),
        ..Default::default()
    }]
}

/// builder is the range builder struct.
pub(crate) struct Builder<'a> {
    pub err: Option<Error>,
    pub sctx: &'a RangerContext,
}

/// `expr.Eval(r.sctx.ExprCtx.GetEvalCtx(), chunk.Row{})`.
// glue: Expression.Eval with an empty row
pub(crate) fn eval_const(sctx: &RangerContext, e: &Expr) -> Result<Datum, Error> {
    let ectx = sctx.expr_ctx.get_eval_ctx();
    e.eval(&*ectx, &EvalState::new(), Row::default())
}

impl<'a> Builder<'a> {
    // glue: builder{sctx: sctx}
    pub fn new(sctx: &'a RangerContext) -> Builder<'a> {
        Builder { err: None, sctx }
    }

    /// build converts Expression on one column into point, which can be further built into Range.
    /// If the input prefixLen is not types.UnspecifiedLength, it means it's for a prefix column in a prefix index. In such
    /// cases, we should cut the prefix and adjust the exclusiveness. Ref: cutPrefixForPoints().
    /// convertToSortKey indicates whether the string values should be converted to sort key.
    /// Converting to sort key can make `like` function be built into Range for new collation column. But we can't restore
    /// the original value from the sort key, so the usage of the result may be limited, like when you need to restore the
    /// result points back to Expression.
    // go: builder.build L1
    pub fn build(
        &mut self,
        expr: &Expr,
        new_tp: &FieldType,
        prefix_len: isize,
        convert_to_sort_key: bool,
    ) -> Vec<Point> {
        match expr {
            Expr::Column(_) => return self.build_from_column(),
            Expr::ScalarFunction(x) => {
                return self.build_from_scalar_func(x, new_tp, prefix_len, convert_to_sort_key);
            }
            Expr::Constant(_) => return self.build_from_constant(expr),
            Expr::CorrelatedColumn(_) => {}
        }

        get_full_range()
    }

    // go: builder.buildFromConstant L1
    fn build_from_constant(&mut self, expr: &Expr) -> Vec<Point> {
        let dt = match eval_const(self.sctx, expr) {
            Ok(dt) => dt,
            Err(err) => {
                self.err = Some(err);
                return Vec::new();
            }
        };
        if dt.is_null() {
            return Vec::new();
        }

        let tc = &self.sctx.type_ctx;
        let (val, err) = dt.to_bool(tc);
        if let Err(err) = err {
            self.err = Some(err);
            return Vec::new();
        }

        if val == 0 {
            return Vec::new();
        }
        get_full_range()
    }

    // go: builder.buildFromColumn L1
    fn build_from_column(&mut self) -> Vec<Point> {
        // column name expression is equivalent to column name is true.
        let start_point1 = pt(min_not_null_datum(), false, true);
        let end_point1 = pt(zero_int(), true, false);
        let start_point2 = pt(zero_int(), true, true);
        let end_point2 = pt(max_value_datum(), false, false);
        vec![start_point1, end_point1, start_point2, end_point2]
    }

    // go: builder.buildFromBinOp L1 [M-GONAME]
    fn build_from_bin_op(
        &mut self,
        expr: &ScalarFunction,
        new_tp: &FieldType,
        prefix_len: isize,
        convert_to_sort_key: bool,
    ) -> Vec<Point> {
        // This has been checked that the binary operation is comparison operation, and one of
        // the operand is column name expression.
        let tc = self.sctx.type_ctx.clone();
        let args = expr.get_args();
        let (col, mut value, mut op): (&tidb_expression::Column, Datum, &'static str);
        if let Expr::Column(c) = &*args[0] {
            col = c;
            value = match eval_const(self.sctx, &args[1]) {
                Ok(v) => v,
                Err(_) => return Vec::new(),
            };
            op = static_func_name(&expr.func_name.l);
        } else {
            let Expr::Column(c) = &*args[1] else {
                return Vec::new();
            };
            col = c;
            value = match eval_const(self.sctx, &args[0]) {
                Ok(v) => v,
                Err(_) => return Vec::new(),
            };
            op = match expr.func_name.l.as_str() {
                ast::GE => ast::LE,
                ast::GT => ast::LT,
                ast::LT => ast::GT,
                ast::LE => ast::GE,
                _ => static_func_name(&expr.func_name.l),
            };
        }
        let ft: &FieldType = &col.ret_type;
        if op != ast::NullEQ && value.is_null() {
            return Vec::new();
        }
        if refine_value_and_op(&tc, col, &mut value, &mut op).is_err() {
            if op == ast::NE {
                // col != an impossible value (not valid year)
                return get_not_null_full_range();
            }
            // col = an impossible value (not valid year)
            return Vec::new();
        }

        let (value, op, is_valid_range) = handle_unsigned_col(ft, value, op);
        if !is_valid_range {
            return Vec::new();
        }

        let (value, op, is_valid_range) = handle_bound_col(ft, value, op);
        if !is_valid_range {
            return Vec::new();
        }

        if ft.get_type() == mysql::TypeEnum && ft.eval_type() == ETString {
            return handle_enum_from_bin_op(&tc, ft, &value, op);
        }

        let mut res: Vec<Point> = Vec::new();
        match op {
            ast::NullEQ | ast::EQ => {
                if op == ast::NullEQ && value.is_null() {
                    res = vec![pt(Datum::default(), false, true), Point::default()]; // [null, null]
                } else {
                    let start_point = pt(value.clone(), false, true);
                    let end_point = pt(value, false, false);
                    res = vec![start_point, end_point];
                }
            }
            ast::NE => {
                let start_point1 = pt(min_not_null_datum(), false, true);
                let end_point1 = pt(value.clone(), true, false);
                let start_point2 = pt(value, true, true);
                let end_point2 = pt(max_value_datum(), false, false);
                res = vec![start_point1, end_point1, start_point2, end_point2];
            }
            ast::LT => {
                let start_point = pt(min_not_null_datum(), false, true);
                let end_point = pt(value, true, false);
                res = vec![start_point, end_point];
            }
            ast::LE => {
                let start_point = pt(min_not_null_datum(), false, true);
                let end_point = pt(value, false, false);
                res = vec![start_point, end_point];
            }
            ast::GT => {
                let start_point = pt(value, true, true);
                let end_point = pt(max_value_datum(), false, false);
                res = vec![start_point, end_point];
            }
            ast::GE => {
                let start_point = pt(value, false, true);
                let end_point = pt(max_value_datum(), false, false);
                res = vec![start_point, end_point];
            }
            _ => {}
        }
        cut_prefix_for_points(&mut res, prefix_len, ft);
        if convert_to_sort_key
            && let Err(err) = convert_points_to_sort_key_in_place(self.sctx, &mut res, new_tp)
        {
            self.err = Some(err);
            return get_full_range();
        }
        res
    }

    // go: builder.buildFromIsTrue L1
    fn build_from_is_true(&mut self, is_not: isize, keep_null: bool) -> Vec<Point> {
        if is_not == 1 {
            if keep_null {
                // Range is {[0, 0]}
                let start_point = pt(zero_int(), false, true);
                let end_point = pt(zero_int(), false, false);
                return vec![start_point, end_point];
            }
            // NOT TRUE range is {[null null] [0, 0]}
            let start_point1 = pt(Datum::default(), false, true);
            let end_point1 = Point::default();
            let start_point2 = pt(zero_int(), false, true);
            let end_point2 = pt(zero_int(), false, false);
            return vec![start_point1, end_point1, start_point2, end_point2];
        }
        // TRUE range is {[-inf 0) (0 +inf]}
        let start_point1 = pt(min_not_null_datum(), false, true);
        let end_point1 = pt(zero_int(), true, false);
        let start_point2 = pt(zero_int(), true, true);
        let end_point2 = pt(max_value_datum(), false, false);
        vec![start_point1, end_point1, start_point2, end_point2]
    }

    // go: builder.buildFromIsFalse L1
    fn build_from_is_false(&mut self, is_not: isize) -> Vec<Point> {
        if is_not == 1 {
            // NOT FALSE range is {[-inf, 0), (0, +inf], [null, null]}
            let start_point1 = pt(Datum::default(), false, true);
            let end_point1 = pt(zero_int(), true, false);
            let start_point2 = pt(zero_int(), true, true);
            let end_point2 = pt(max_value_datum(), false, false);
            return vec![start_point1, end_point1, start_point2, end_point2];
        }
        // FALSE range is {[0, 0]}
        let start_point = pt(zero_int(), false, true);
        let end_point = pt(zero_int(), false, false);
        vec![start_point, end_point]
    }

    // go: builder.buildFromIn L1
    fn build_from_in(
        &mut self,
        expr: &ScalarFunction,
        new_tp: &FieldType,
        prefix_len: isize,
        convert_to_sort_key: bool,
    ) -> (Vec<Point>, bool) {
        let list = &expr.get_args()[1..];
        let mut range_points: Vec<Point> = Vec::with_capacity(list.len() * 2);
        let mut has_null = false;
        let eval_ctx = self.sctx.expr_ctx.get_eval_ctx();
        let ft = expr.get_args()[0].get_type(&*eval_ctx).into_owned();
        let col_collate = ft.get_collate().to_string();
        let tc = self.sctx.type_ctx.clone();
        for e in list {
            if !matches!(&**e, Expr::Constant(_)) {
                self.err = Some(ErrUnsupportedType.gen_with_stack(
                    "expr:%v is not constant",
                    vec![Arg::from(lossy(
                        e.string_with_ctx(&*eval_ctx, RedactLogDisable),
                    ))],
                ));
                return (get_full_range(), has_null);
            }
            let mut dt = match eval_const(self.sctx, e) {
                Ok(dt) => dt,
                Err(_) => {
                    self.err = Some(ErrUnsupportedType.gen_with_stack(
                        "expr:%v is not evaluated",
                        vec![Arg::from(lossy(
                            e.string_with_ctx(&*eval_ctx, RedactLogDisable),
                        ))],
                    ));
                    return (get_full_range(), has_null);
                }
            };
            if dt.is_null() {
                has_null = true;
                continue;
            }
            if ft.get_type() == mysql::TypeEnum {
                let mut err: Result<(), Error> = Ok(());
                match dt.kind() {
                    KindString | KindBytes | KindBinaryLiteral => {
                        // Can't use ConvertTo directly, since we shouldn't convert numerical string to Enum in select stmt.
                        match parse_enum_name(
                            ft.get_elems().unwrap_or(&[]),
                            dt.get_string(),
                            ft.get_collate(),
                        ) {
                            Ok(e) => dt.set_mysql_enum(e, ft.get_collate().to_string()),
                            Err(parse_err) => err = Err(parse_err),
                        }
                    }
                    _ => {
                        let (converted, e) = dt.convert_to(&tc, &ft);
                        dt = converted;
                        err = e;
                    }
                }

                if err.is_err() {
                    // in (..., an impossible value (not valid enum), ...), the range is empty, so skip it.
                    continue;
                }
            }
            if ft.get_type() == mysql::TypeYear {
                let (converted, err) = dt.convert_to_mysql_year(&tc, &ft);
                dt = converted;
                if err.is_err() {
                    // in (..., an impossible value (not valid year), ...), the range is empty, so skip it.
                    continue;
                }
            }
            if ft.eval_type() == ETString
                && (dt.kind() == KindString || dt.kind() == KindBinaryLiteral)
            {
                let s = dt.get_string().to_vec();
                dt.set_string(s, ft.get_collate().to_string()); // refine the string like what we did in builder.buildFromBinOp
            }
            let start_point = pt(dt.clone(), false, true);
            let end_point = pt(dt, false, false);
            range_points.push(start_point);
            range_points.push(end_point);
        }
        let collator = collate::get_collator(&col_collate);
        let mut sort_err: Option<Result<(), Error>> = None;
        sort(&mut range_points, |a, b| {
            let (cmpare, err) = range_point_cmp(&tc, a, b, collator);
            sort_err = Some(err);
            cmpare < 0
        });
        // `r.err` is assigned by every comparison (the last one wins).
        if let Some(err) = sort_err {
            self.err = err.err();
        }
        // check and remove duplicates
        let (mut cur_pos, mut front_pos) = (0usize, 0usize);
        while front_pos < range_points.len() {
            if range_points[cur_pos].start == range_points[front_pos].start {
                front_pos += 1;
            } else {
                cur_pos += 1;
                range_points.swap(cur_pos, front_pos);
                front_pos += 1;
            }
        }
        if cur_pos > 0 {
            cur_pos += 1;
        }
        range_points.truncate(cur_pos);
        cut_prefix_for_points(&mut range_points, prefix_len, &ft);
        if convert_to_sort_key
            && let Err(err) =
                convert_points_to_sort_key_in_place(self.sctx, &mut range_points, new_tp)
        {
            self.err = Some(err);
            return (get_full_range(), false);
        }
        (range_points, has_null)
    }

    // go: builder.newBuildFromPatternLike L1
    fn new_build_from_pattern_like(
        &mut self,
        expr: &ScalarFunction,
        new_tp: &FieldType,
        prefix_len: isize,
        convert_to_sort_key: bool,
    ) -> Vec<Point> {
        let (_, collation) = expr.charset_and_collation();
        let collation = collation.to_string();
        let eval_ctx = self.sctx.expr_ctx.get_eval_ctx();
        let args = expr.get_args();
        if !collate::compatible_collate(args[0].get_type(&*eval_ctx).get_collate(), &collation) {
            return get_full_range();
        }
        let pdt = eval_const(self.sctx, &args[1]);
        let tp_of_pattern = args[0].get_type(&*eval_ctx).into_owned();
        let pdt = match pdt {
            Ok(d) => d,
            Err(err) => {
                self.err = Some(err);
                return get_full_range();
            }
        };
        let pattern = match pdt.to_string() {
            Ok(p) => p,
            Err(err) => {
                self.err = Some(err);
                return get_full_range();
            }
        };
        // non-exceptional return case 1: empty pattern
        if pattern.is_empty() {
            let start_point = pt(new_string_datum(Vec::new()), false, true);
            let end_point = pt(new_string_datum(Vec::new()), false, false);
            let mut res = vec![start_point, end_point];
            if convert_to_sort_key
                && let Err(err) = convert_points_to_sort_key_in_place(self.sctx, &mut res, new_tp)
            {
                self.err = Some(err);
                return get_full_range();
            }
            return res;
        }
        let mut low_value: Vec<u8> = Vec::with_capacity(pattern.len());
        let edt = match eval_const(self.sctx, &args[2]) {
            Ok(d) => d,
            Err(err) => {
                self.err = Some(err);
                return get_full_range();
            }
        };
        let escape = edt.get_int64() as u8;
        let mut exclude = false;
        let mut is_exact_match = true;
        let mut i = 0usize;
        while i < pattern.len() {
            if pattern[i] == escape {
                i += 1;
                if i < pattern.len() {
                    low_value.push(pattern[i]);
                } else {
                    low_value.push(escape);
                }
                i += 1;
                continue;
            }
            if pattern[i] == b'%' {
                // Get the prefix.
                is_exact_match = false;
                break;
            } else if pattern[i] == b'_' {
                // Get the prefix, but exclude the prefix.
                // e.g., "abc_x", the start point excludes "abc" because the string length is more than 3.
                //
                // However, like the similar check in (*conditionChecker).checkLikeFunc(), in tidb's implementation, for
                // PAD SPACE collations, the trailing spaces are removed in the index key. So we are unable to distinguish
                // 'xxx' from 'xxx   ' by a single index range scan. If we exclude the start point for PAD SPACE collation,
                // we will actually miss 'xxx   ', which will cause wrong results.
                if !collate::is_pad_space_collation(&collation) {
                    exclude = true;
                }
                is_exact_match = false;
                break;
            }
            low_value.push(pattern[i]);
            i += 1;
        }
        // non-exceptional return case 2: no characters before the wildcard
        if low_value.is_empty() {
            return vec![
                pt(min_not_null_datum(), false, true),
                pt(max_value_datum(), false, false),
            ];
        }
        // non-exceptional return case 3: pattern contains valid characters and doesn't contain the wildcard
        if is_exact_match {
            let val =
                new_collation_string_datum(low_value, tp_of_pattern.get_collate().to_string());
            let start_point = pt(val.clone(), false, true);
            let end_point = pt(val, false, false);
            let mut res = vec![start_point, end_point];
            cut_prefix_for_points(&mut res, prefix_len, &tp_of_pattern);
            if convert_to_sort_key
                && let Err(err) = convert_points_to_sort_key_in_place(self.sctx, &mut res, new_tp)
            {
                self.err = Some(err);
                return get_full_range();
            }
            return res;
        }

        // non-exceptional return case 4: pattern contains valid characters and contains the wildcard

        // non-exceptional return case 4-1
        // If it's not a _bin or binary collation, and we don't convert the value to the sort key, we can't build
        // a range for the wildcard.
        if !convert_to_sort_key && !collate::is_bin_collation(tp_of_pattern.get_collate()) {
            return vec![
                pt(min_not_null_datum(), false, true),
                pt(max_value_datum(), false, false),
            ];
        }

        // non-exceptional return case 4-2: build a range for the wildcard
        // the end_key is sortKey(start_value) + 1
        let mut original_start_point = pt(Datum::default(), exclude, true);
        original_start_point.value.set_bytes_as_string(
            low_value,
            tp_of_pattern.get_collate().to_string(),
            tp_of_pattern.get_flen() as u32,
        );
        cut_prefix_for_points(
            std::slice::from_mut(&mut original_start_point),
            prefix_len,
            &tp_of_pattern,
        );

        // If we don't trim the trailing spaces, which means using KeyWithoutTrimRightSpace() instead of Key(), we can build
        // a smaller range for better performance, e.g., LIKE '  %'.
        // However, if it's a PAD SPACE collation, we must trim the trailing spaces for the start point to ensure the correctness.
        // Because the trailing spaces are trimmed in the stored index key. For example, for LIKE 'abc  %' on utf8mb4_bin
        // column, the start key should be 'abd' instead of 'abc ', but the end key can be 'abc!'. ( ' ' is 32 and '!' is 33
        // in ASCII)
        let should_trim_trailing_space = collate::is_pad_space_collation(&collation);
        let mut start_point = original_start_point.clone();
        if let Err(err) = convert_point_to_sort_key_in_place(
            self.sctx,
            &mut start_point,
            new_tp,
            should_trim_trailing_space,
        ) {
            self.err = Some(err);
            return get_full_range();
        }
        let mut sort_key_point_without_trim = original_start_point;
        if let Err(err) = convert_point_to_sort_key_in_place(
            self.sctx,
            &mut sort_key_point_without_trim,
            new_tp,
            false,
        ) {
            self.err = Some(err);
            return get_full_range();
        }
        let mut sort_key_without_trim = sort_key_point_without_trim.value.get_bytes().to_vec();
        let mut end_point = pt(max_value_datum(), true, false);
        let mut i = sort_key_without_trim.len() as isize - 1;
        while i >= 0 {
            let iu = i as usize;
            // Make the end point value more than the start point value,
            // and the length of the end point value is the same as the length of the start point value.
            // e.g., the start point value is "abc", so the end point value is "abd".
            sort_key_without_trim[iu] = sort_key_without_trim[iu].wrapping_add(1);
            if sort_key_without_trim[iu] != 0 {
                end_point.value.set_bytes(sort_key_without_trim.clone());
                break;
            }
            // If sortKeyWithoutTrim[i] is 255 and sortKeyWithoutTrim[i]++ is 0, then the end point value is max value.
            if i == 0 {
                end_point.value = max_value_datum();
            }
            i -= 1;
        }
        vec![start_point, end_point]
    }

    // go: builder.buildFromNot L1
    fn build_from_not(
        &mut self,
        expr: &ScalarFunction,
        new_tp: &FieldType,
        prefix_len: isize,
        convert_to_sort_key: bool,
    ) -> Vec<Point> {
        match expr.func_name.l.as_str() {
            ast::IsTruthWithoutNull => return self.build_from_is_true(1, false),
            ast::IsTruthWithNull => return self.build_from_is_true(1, true),
            ast::IsFalsity => return self.build_from_is_false(1),
            ast::In => {
                let mut is_unsigned_int_col = false;
                // Note that we must handle the cutting prefix and converting to sort key in buildFromNot, because if we cut the
                // prefix inside buildFromIn(), the inversion logic here would make an incomplete and wrong range.
                // For example, for index col(1), col NOT IN ('aaa', 'bbb'), if we cut the prefix in buildFromIn(), we would get
                // ['a', 'a'], ['b', 'b'] from there. Then after in this function we would get ['' 'a'), ('a', 'b'), ('b', +inf]
                // as the result. This is wrong because data like 'ab' would be missed. Actually we are unable to build a range
                // for this case.
                // So we must cut the prefix in this function, therefore converting to sort key must also be done here.
                let (mut range_points, has_null) =
                    self.build_from_in(expr, new_tp, UnspecifiedLength, false);
                if has_null {
                    return Vec::new();
                }
                if let Expr::Column(x) = &*expr.get_args()[0] {
                    is_unsigned_int_col = mysql::has_unsigned_flag(x.ret_type.get_flag())
                        && mysql::is_integer_type(x.ret_type.get_type());
                }
                // negative ranges can be directly ignored for unsigned int columns.
                if is_unsigned_int_col {
                    let mut non_negative_pos = 0usize;
                    while non_negative_pos < range_points.len() {
                        if range_points[non_negative_pos].value.kind() == KindUint64
                            || range_points[non_negative_pos].value.get_int64() >= 0
                        {
                            break;
                        }
                        non_negative_pos += 2;
                    }
                    range_points.drain(..non_negative_pos.min(range_points.len()));
                }
                let mut ret_range_points: Vec<Point> = Vec::with_capacity(2 + range_points.len());
                let mut previous_value = Datum::default();
                let mut i = 0usize;
                while i < range_points.len() {
                    ret_range_points.push(pt(previous_value.clone(), true, true));
                    ret_range_points.push(pt(range_points[i].value.clone(), true, false));
                    previous_value = range_points[i].value.clone();
                    i += 2;
                }
                // Append the interval (last element, max value].
                ret_range_points.push(pt(previous_value, true, true));
                ret_range_points.push(pt(max_value_datum(), false, false));
                let eval_ctx = self.sctx.expr_ctx.get_eval_ctx();
                let tp0 = expr.get_args()[0].get_type(&*eval_ctx).into_owned();
                cut_prefix_for_points(&mut ret_range_points, prefix_len, &tp0);
                if convert_to_sort_key
                    && let Err(err) = convert_points_to_sort_key_in_place(
                        self.sctx,
                        &mut ret_range_points,
                        new_tp,
                    )
                {
                    self.err = Some(err);
                    return get_full_range();
                }
                return ret_range_points;
            }
            ast::Like => {
                // Pattern not like is not supported.
                self.err =
                    Some(ErrUnsupportedType.gen_with_stack("NOT LIKE is not supported.", vec![]));
                return get_full_range();
            }
            ast::IsNull => {
                let start_point = pt(min_not_null_datum(), false, true);
                let end_point = pt(max_value_datum(), false, false);
                return vec![start_point, end_point];
            }
            _ => {}
        }
        // TODO: currently we don't handle ast.LogicAnd, ast.LogicOr, ast.GT, ast.LT and so on. Most of those cases are eliminated
        // by PushDownNot but they may happen. For now, we return full range for those unhandled cases in order to keep correctness.
        // Later we need to cover those cases and set r.err when meeting some unexpected case.
        get_full_range()
    }

    // go: builder.buildFromScalarFunc L1
    fn build_from_scalar_func(
        &mut self,
        expr: &ScalarFunction,
        new_tp: &FieldType,
        prefix_len: isize,
        convert_to_sort_key: bool,
    ) -> Vec<Point> {
        match expr.func_name.l.as_str() {
            ast::GE | ast::GT | ast::LT | ast::LE | ast::EQ | ast::NE | ast::NullEQ => {
                return self.build_from_bin_op(expr, new_tp, prefix_len, convert_to_sort_key);
            }
            ast::LogicAnd => {
                let mut collator = collate::get_collator(new_tp.get_collate());
                if convert_to_sort_key {
                    collator = collate::get_collator(CollationBin);
                }
                let a = self.build(&expr.get_args()[0], new_tp, prefix_len, convert_to_sort_key);
                let b = self.build(&expr.get_args()[1], new_tp, prefix_len, convert_to_sort_key);
                return self.intersection(a, b, collator);
            }
            ast::LogicOr => {
                let mut collator = collate::get_collator(new_tp.get_collate());
                if convert_to_sort_key {
                    collator = collate::get_collator(CollationBin);
                }
                let a = self.build(&expr.get_args()[0], new_tp, prefix_len, convert_to_sort_key);
                let b = self.build(&expr.get_args()[1], new_tp, prefix_len, convert_to_sort_key);
                return self.union(a, b, collator);
            }
            ast::IsTruthWithoutNull => return self.build_from_is_true(0, false),
            ast::IsTruthWithNull => return self.build_from_is_true(0, true),
            ast::IsFalsity => return self.build_from_is_false(0),
            ast::In => {
                let (ret_points, _) =
                    self.build_from_in(expr, new_tp, prefix_len, convert_to_sort_key);
                return ret_points;
            }
            ast::Like => {
                return self.new_build_from_pattern_like(
                    expr,
                    new_tp,
                    prefix_len,
                    convert_to_sort_key,
                );
            }
            ast::IsNull => {
                let start_point = pt(Datum::default(), false, true);
                let end_point = Point::default();
                return vec![start_point, end_point];
            }
            ast::UnaryNot => {
                let Expr::ScalarFunction(arg) = &*expr.get_args()[0] else {
                    // Go: a failed type assertion `.(*expression.ScalarFunction)` panics.
                    panic!(
                        "interface conversion: expression.Expression is not *expression.ScalarFunction"
                    );
                };
                return self.build_from_not(arg, new_tp, prefix_len, convert_to_sort_key);
            }
            _ => {}
        }

        Vec::new()
    }

    /// We need an input collator because our (*Datum).Compare(), which is used in this method, needs an explicit collator
    /// input to handle comparison for string and bytes.
    /// Note that if the points are converted to sort key, the collator should be set to charset.CollationBin.
    // go: builder.intersection L1
    pub fn intersection(
        &mut self,
        a: Vec<Point>,
        b: Vec<Point>,
        collator: &dyn Collator,
    ) -> Vec<Point> {
        self.merge(a, b, false, collator)
    }

    /// We need an input collator because our (*Datum).Compare(), which is used in this method, needs an explicit collator
    /// input to handle comparison for string and bytes.
    /// Note that if the points are converted to sort key, the collator should be set to charset.CollationBin.
    // go: builder.union L1
    pub fn union(&mut self, a: Vec<Point>, b: Vec<Point>, collator: &dyn Collator) -> Vec<Point> {
        self.merge(a, b, true, collator)
    }

    // go: builder.mergeSorted L1
    fn merge_sorted(
        &mut self,
        a: Vec<Point>,
        b: Vec<Point>,
        collator: &dyn Collator,
    ) -> Vec<Point> {
        let mut ret: Vec<Point> = Vec::with_capacity(a.len() + b.len());
        let (alen, blen) = (a.len(), b.len());
        let mut a = a.into_iter().peekable();
        let mut b = b.into_iter().peekable();
        let (mut i, mut j) = (0usize, 0usize);
        let tc = self.sctx.type_ctx.clone();
        while i < alen && j < blen {
            let (cmp, err) = range_point_cmp(&tc, a.peek().unwrap(), b.peek().unwrap(), collator);
            if let Err(err) = err {
                self.err = Some(err);
                return Vec::new();
            }
            if cmp < 0 {
                ret.push(a.next().unwrap());
                i += 1;
            } else {
                ret.push(b.next().unwrap());
                j += 1;
            }
        }
        if i < alen {
            ret.extend(a);
        } else if j < blen {
            ret.extend(b);
        }
        ret
    }

    // go: builder.merge L1
    fn merge(
        &mut self,
        a: Vec<Point>,
        b: Vec<Point>,
        union: bool,
        collator: &dyn Collator,
    ) -> Vec<Point> {
        let merged_points = self.merge_sorted(a, b, collator);
        if self.err.is_some() {
            return Vec::new();
        }

        let mut in_range_count: isize = 0;
        let required_in_range_count: isize = if union { 1 } else { 2 };
        let mut out: Vec<Point> = Vec::with_capacity(merged_points.len());
        for val in merged_points {
            if val.start {
                in_range_count += 1;
                if in_range_count == required_in_range_count {
                    // Just reached the required in range count, a new range started.
                    out.push(val);
                }
            } else {
                let end = in_range_count == required_in_range_count;
                in_range_count -= 1;
                if end {
                    // Just about to leave the required in range count, the range is ended.
                    out.push(val);
                }
            }
        }
        out
    }
}

/// `refineValueAndOp` of `buildFromBinOp` (a closure in Go) refines the constant datum and operator:
/// 1. for string type since we may eval the constant to another collation instead of its own collation.
/// 2. for year type since 2-digit year value need adjustment, see https://dev.mysql.com/doc/refman/5.6/en/year.html
// glue: the refineValueAndOp closure of builder.buildFromBinOp (its body is ported with the function)
fn refine_value_and_op(
    tc: &TypeContext,
    col: &tidb_expression::Column,
    value: &mut Datum,
    op: &mut &'static str,
) -> Result<(), Error> {
    if col.ret_type.eval_type() == ETString
        && (value.kind() == KindString || value.kind() == KindBinaryLiteral)
    {
        let s = value.get_string().to_vec();
        value.set_string(s, col.ret_type.get_collate().to_string());
    }
    // If nulleq with null value, values.ToInt64 will return err
    if col.ret_type.get_type() == mysql::TypeYear && !value.is_null() {
        // Convert the out-of-range uint number to int and then let the following logic can handle it correctly.
        // Since the max value of year is 2155, `col op MaxUint` should have the same result with `col op MaxInt`.
        if value.kind() == KindUint64 && value.get_uint64() > i64::MAX as u64 {
            value.set_int64(i64::MAX);
        }

        // If the original value is adjusted, we need to change the condition.
        // For example, col < 2156. Since the max year is 2155, 2156 is changed to 2155.
        // col < 2155 is wrong. It should be col <= 2155.
        let (pre_value, err1) = value.to_int64(tc);
        err1?;
        let (converted, mut err) = value.convert_to_mysql_year(tc, &col.ret_type);
        *value = converted;
        if let Err(e) = &err
            && tidb_base::errors::error_equal(
                Some(e),
                Some(&tidb_types::errors::ErrWarnDataOutOfRange),
            )
        {
            // Keep err for EQ and NE.
            match *op {
                ast::GT => {
                    if value.get_int64() > pre_value {
                        *op = ast::GE;
                    }
                    err = Ok(());
                }
                ast::LT => {
                    if value.get_int64() < pre_value {
                        *op = ast::LE;
                    }
                    err = Ok(());
                }
                ast::GE | ast::LE => err = Ok(()),
                _ => {}
            }
        }
        return err;
    }
    Ok(())
}

/// The `&'static str` of a comparison function name (`expr.FuncName.L`).
// glue: Go string value of FuncName.L as an operator constant
fn static_func_name(l: &str) -> &'static str {
    match l {
        ast::EQ => ast::EQ,
        ast::NE => ast::NE,
        ast::LT => ast::LT,
        ast::LE => ast::LE,
        ast::GT => ast::GT,
        ast::GE => ast::GE,
        ast::NullEQ => ast::NullEQ,
        _ => "",
    }
}

/// handleUnsignedCol handles the case when unsigned column meets negative value.
/// The three returned values are: fixed constant value, fixed operator, and a boolean
/// which indicates whether the range is valid or not.
// go: handleUnsignedCol L1
fn handle_unsigned_col(
    ft: &FieldType,
    mut val: Datum,
    mut op: &'static str,
) -> (Datum, &'static str, bool) {
    let is_unsigned = mysql::has_unsigned_flag(ft.get_flag());
    let is_negative = (val.kind() == KindInt64 && val.get_int64() < 0)
        || (val.kind() == KindFloat32 && val.get_float32() < 0.0)
        || (val.kind() == KindFloat64 && val.get_float64() < 0.0)
        || (val.kind() == KindMysqlDecimal && val.get_mysql_decimal().is_negative());

    if !is_unsigned || !is_negative {
        return (val, op, true);
    }

    // If the operator is GT, GE or NE, the range should be [0, +inf].
    // Otherwise the value is out of valid range.
    if op == ast::GT || op == ast::GE || op == ast::NE {
        op = ast::GE;
        match val.kind() {
            KindInt64 => val.set_uint64(0),
            KindFloat32 => val.set_float32(0.0),
            KindFloat64 => val.set_float64(0.0),
            KindMysqlDecimal => val.set_mysql_decimal(MyDecimal::default()),
            _ => {}
        }
        return (val, op, true);
    }

    (val, op, false)
}

/// handleBoundCol handles the case when column meets overflow value.
/// The three returned values are: fixed constant value, fixed operator, and a boolean
/// which indicates whether the range is valid or not.
// go: handleBoundCol L1
fn handle_bound_col(
    ft: &FieldType,
    mut val: Datum,
    mut op: &'static str,
) -> (Datum, &'static str, bool) {
    let is_unsigned = mysql::has_unsigned_flag(ft.get_flag());
    let is_negative = val.kind() == KindInt64 && val.get_int64() < 0;
    if is_unsigned {
        return (val, op, true);
    }

    match ft.get_type() {
        mysql::TypeTiny
        | mysql::TypeShort
        | mysql::TypeInt24
        | mysql::TypeLong
        | mysql::TypeLonglong => {
            if !is_negative && val.get_uint64() > i64::MAX as u64 {
                match op {
                    ast::GT | ast::GE => return (val, op, false),
                    ast::NE | ast::LE | ast::LT => {
                        op = ast::LE;
                        val = new_int_datum(i64::MAX);
                    }
                    _ => {}
                }
            }
        }
        mysql::TypeFloat => {
            if val.get_float64() > f32::MAX as f64 {
                match op {
                    ast::GT | ast::GE => return (val, op, false),
                    ast::NE | ast::LE | ast::LT => {
                        op = ast::LE;
                        val = new_float32_datum(f32::MAX);
                    }
                    _ => {}
                }
            } else if val.get_float64() < -(f32::MAX as f64) {
                match op {
                    ast::LE | ast::LT => return (val, op, false),
                    ast::GT | ast::GE | ast::NE => {
                        op = ast::GE;
                        val = new_float32_datum(-f32::MAX);
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }

    (val, op, true)
}

// go: handleEnumFromBinOp L1
fn handle_enum_from_bin_op(
    tc: &TypeContext,
    ft: &FieldType,
    val: &Datum,
    op: &'static str,
) -> Vec<Point> {
    let elems = ft.get_elems().unwrap_or(&[]);
    let mut res: Vec<Point> = Vec::with_capacity(elems.len() * 2);
    let append_point_func = |res: &mut Vec<Point>, d: &Datum| {
        res.push(pt(d.clone(), false, true));
        res.push(pt(d.clone(), false, false));
    };

    if op == ast::NullEQ && val.is_null() {
        res.push(pt(Datum::default(), false, true));
        res.push(Point::default()); // null point
    }

    let mut tmp_enum = Enum::default();
    for i in 0..=elems.len() {
        if i == 0 {
            tmp_enum = Enum::default();
        } else {
            tmp_enum.name = elems[i - 1].clone();
            tmp_enum.value = i as u64;
        }

        let d = new_collate_mysql_enum_datum(tmp_enum.clone(), ft.get_collate().to_string());
        let (v, err) = d.compare(tc, val, collate::get_collator(ft.get_collate()));
        if err.is_ok() {
            // Go's switch on op, each case appending the point when its comparison holds.
            let hit = match op {
                ast::LT => v < 0,
                ast::LE => v <= 0,
                ast::GT => v > 0,
                ast::GE => v >= 0,
                ast::EQ | ast::NullEQ => v == 0,
                ast::NE => v != 0,
                _ => false,
            };
            if hit {
                append_point_func(&mut res, &d);
            }
        }
    }
    res
}

/// Go `string(bytes)` of an expression's text for an error message.
// glue: []byte -> string for gofmt
pub(crate) fn lossy(b: Vec<u8>) -> String {
    String::from_utf8_lossy(&b).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `point.String` (Go has no test of it; its four forms and the infinities).
    #[test]
    fn point_string() {
        let p = |value: Datum, excl: bool, start: bool| Point { value, excl, start };
        assert_eq!(p(new_int_datum(1), false, true).string(), b"[1");
        assert_eq!(p(new_int_datum(1), true, true).string(), b"(1");
        assert_eq!(p(new_int_datum(2), false, false).string(), b"2]");
        assert_eq!(p(new_int_datum(2), true, false).string(), b"2)");
        assert_eq!(p(min_not_null_datum(), false, true).string(), b"[-inf");
        assert_eq!(p(max_value_datum(), false, false).string(), b"+inf]");
        assert_eq!(p(Datum::default(), false, true).string(), b"[<nil>");
        assert_eq!(
            p(new_string_datum(b"ab".to_vec()), true, true).string(),
            b"(ab"
        );
    }
}
