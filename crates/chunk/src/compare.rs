//! go-file: pkg/util/chunk/compare.go
//!
//! Row value comparison (N5a 3.1: L1). `cmpBit` calls `types.BinaryLiteral.Compare` through
//! its borrowed-bytes form [`binary_literal::compare_bytes`] (a comparator must not allocate
//! per call).

// Datum kinds keep their Go names (mapping rule M-GONAME).
#![allow(non_upper_case_globals)]

use std::cmp::Ordering;

use tidb_base::mysql;
use tidb_parser::types::FieldType;
use tidb_types::binary_literal;
use tidb_types::collate;
use tidb_types::compare::compare_string;
use tidb_types::datum::Datum;
use tidb_types::datum::{
    KindBinaryLiteral, KindBytes, KindFloat32, KindFloat64, KindInt64, KindMaxValue,
    KindMinNotNull, KindMysqlBit, KindMysqlDecimal, KindMysqlDuration, KindMysqlEnum,
    KindMysqlJSON, KindMysqlSet, KindMysqlTime, KindNull, KindString, KindUint64,
    KindVectorFloat32,
};
use tidb_types::json::compare_binary_json;

use crate::chunk::Chunk;
use crate::row::Row;

/// CompareFunc is a function to compare the two values in Row, the two columns must have the same type.
pub type CompareFunc = Box<dyn Fn(Row<'_>, usize, Row<'_>, usize) -> isize + Send + Sync>;

/// GetCompareFunc gets a compare function for the field type.
// go: GetCompareFunc L1
pub fn get_compare_func(tp: &FieldType) -> Option<CompareFunc> {
    let f: CompareFunc = match tp.get_type() {
        mysql::TypeTiny
        | mysql::TypeShort
        | mysql::TypeInt24
        | mysql::TypeLong
        | mysql::TypeLonglong
        | mysql::TypeYear => {
            if mysql::has_unsigned_flag(tp.get_flag()) {
                Box::new(cmp_uint64)
            } else {
                Box::new(cmp_int64)
            }
        }
        mysql::TypeFloat => Box::new(cmp_float32),
        mysql::TypeDouble => Box::new(cmp_float64),
        mysql::TypeString
        | mysql::TypeVarString
        | mysql::TypeVarchar
        | mysql::TypeBlob
        | mysql::TypeTinyBlob
        | mysql::TypeMediumBlob
        | mysql::TypeLongBlob => gen_cmp_string_func(tp.get_collate().to_owned()),
        mysql::TypeDate | mysql::TypeDatetime | mysql::TypeTimestamp => Box::new(cmp_time),
        mysql::TypeDuration => Box::new(cmp_duration),
        mysql::TypeNewDecimal => Box::new(cmp_my_decimal),
        mysql::TypeSet | mysql::TypeEnum => Box::new(cmp_name_value),
        mysql::TypeBit => Box::new(cmp_bit),
        mysql::TypeJSON => Box::new(cmp_json),
        mysql::TypeTiDBVectorFloat32 => Box::new(cmp_vector_float32),
        mysql::TypeNull => Box::new(cmp_null_const),
        _ => return None,
    };
    Some(f)
}

/// Go's `cmp.Compare` for ordered values: NaN is less than every number and equal to NaN.
// go-std: cmp.Compare L1
pub(crate) fn go_cmp<T: PartialOrd>(x: T, y: T) -> isize {
    #[allow(clippy::eq_op)]
    let (x_nan, y_nan) = (x != x, y != y);
    if x_nan {
        if y_nan {
            return 0;
        }
        return -1;
    }
    if y_nan {
        return 1;
    }
    match x.partial_cmp(&y) {
        Some(Ordering::Less) => -1,
        Some(Ordering::Greater) => 1,
        _ => 0,
    }
}

// go: cmpNull L1
fn cmp_null(l_null: bool, r_null: bool) -> isize {
    if l_null && r_null {
        return 0;
    }
    if l_null {
        return -1;
    }
    1
}

// go: cmpInt64 L1
fn cmp_int64(l: Row<'_>, l_col: usize, r: Row<'_>, r_col: usize) -> isize {
    let (l_null, r_null) = (l.is_null(l_col), r.is_null(r_col));
    if l_null || r_null {
        return cmp_null(l_null, r_null);
    }
    go_cmp(l.get_int64(l_col), r.get_int64(r_col))
}

// go: cmpUint64 L1
fn cmp_uint64(l: Row<'_>, l_col: usize, r: Row<'_>, r_col: usize) -> isize {
    let (l_null, r_null) = (l.is_null(l_col), r.is_null(r_col));
    if l_null || r_null {
        return cmp_null(l_null, r_null);
    }
    go_cmp(l.get_uint64(l_col), r.get_uint64(r_col))
}

// go: genCmpStringFunc L1
fn gen_cmp_string_func(collation: String) -> CompareFunc {
    Box::new(move |l, l_col, r, r_col| {
        cmp_string_with_collation_info(l, l_col, r, r_col, &collation)
    })
}

// go: cmpStringWithCollationInfo L1
fn cmp_string_with_collation_info(
    l: Row<'_>,
    l_col: usize,
    r: Row<'_>,
    r_col: usize,
    collation: &str,
) -> isize {
    let (l_null, r_null) = (l.is_null(l_col), r.is_null(r_col));
    if l_null || r_null {
        return cmp_null(l_null, r_null);
    }
    // types.CompareString(x, y, collation) = collate.GetCollator(collation).Compare(x, y)
    collate::get_collator(collation).compare(l.get_string(l_col), r.get_string(r_col))
}

// go: cmpFloat32 L1
fn cmp_float32(l: Row<'_>, l_col: usize, r: Row<'_>, r_col: usize) -> isize {
    let (l_null, r_null) = (l.is_null(l_col), r.is_null(r_col));
    if l_null || r_null {
        return cmp_null(l_null, r_null);
    }
    go_cmp(l.get_float32(l_col) as f64, r.get_float32(r_col) as f64)
}

// go: cmpFloat64 L1
fn cmp_float64(l: Row<'_>, l_col: usize, r: Row<'_>, r_col: usize) -> isize {
    let (l_null, r_null) = (l.is_null(l_col), r.is_null(r_col));
    if l_null || r_null {
        return cmp_null(l_null, r_null);
    }
    go_cmp(l.get_float64(l_col), r.get_float64(r_col))
}

// go: cmpMyDecimal L1
fn cmp_my_decimal(l: Row<'_>, l_col: usize, r: Row<'_>, r_col: usize) -> isize {
    let (l_null, r_null) = (l.is_null(l_col), r.is_null(r_col));
    if l_null || r_null {
        return cmp_null(l_null, r_null);
    }
    let (l_dec, r_dec) = (l.get_my_decimal(l_col), r.get_my_decimal(r_col));
    l_dec.compare(&r_dec)
}

// go: cmpTime L1
fn cmp_time(l: Row<'_>, l_col: usize, r: Row<'_>, r_col: usize) -> isize {
    let (l_null, r_null) = (l.is_null(l_col), r.is_null(r_col));
    if l_null || r_null {
        return cmp_null(l_null, r_null);
    }
    let (l_time, r_time) = (l.get_time(l_col), r.get_time(r_col));
    l_time.compare(&r_time)
}

// go: cmpDuration L1
fn cmp_duration(l: Row<'_>, l_col: usize, r: Row<'_>, r_col: usize) -> isize {
    let (l_null, r_null) = (l.is_null(l_col), r.is_null(r_col));
    if l_null || r_null {
        return cmp_null(l_null, r_null);
    }
    let (l_dur, r_dur) = (
        l.get_duration(l_col, 0).duration,
        r.get_duration(r_col, 0).duration,
    );
    go_cmp(l_dur, r_dur)
}

// go: cmpNameValue L1
fn cmp_name_value(l: Row<'_>, l_col: usize, r: Row<'_>, r_col: usize) -> isize {
    let (l_null, r_null) = (l.is_null(l_col), r.is_null(r_col));
    if l_null || r_null {
        return cmp_null(l_null, r_null);
    }
    let (_, l_val) = l.get_name_value(l_col);
    let (_, r_val) = r.get_name_value(r_col);
    go_cmp(l_val, r_val)
}

// go: cmpBit L1
fn cmp_bit(l: Row<'_>, l_col: usize, r: Row<'_>, r_col: usize) -> isize {
    let (l_null, r_null) = (l.is_null(l_col), r.is_null(r_col));
    if l_null || r_null {
        return cmp_null(l_null, r_null);
    }
    binary_literal::compare_bytes(l.get_bytes(l_col), r.get_bytes(r_col))
}

// go: cmpJSON L1
fn cmp_json(l: Row<'_>, l_col: usize, r: Row<'_>, r_col: usize) -> isize {
    let (l_null, r_null) = (l.is_null(l_col), r.is_null(r_col));
    if l_null || r_null {
        return cmp_null(l_null, r_null);
    }
    let (l_j, r_j) = (l.get_json(l_col), r.get_json(r_col));
    compare_binary_json(l_j, r_j)
}

// go: cmpNullConst L1
fn cmp_null_const(_: Row<'_>, _: usize, _: Row<'_>, _: usize) -> isize {
    0
}

// go: cmpVectorFloat32 L1
fn cmp_vector_float32(l: Row<'_>, l_col: usize, r: Row<'_>, r_col: usize) -> isize {
    let (l_null, r_null) = (l.is_null(l_col), r.is_null(r_col));
    if l_null || r_null {
        return cmp_null(l_null, r_null);
    }
    let (lv, rv) = (l.get_vector_float32(l_col), r.get_vector_float32(r_col));
    lv.compare(&rv)
}

/// Compare compares the value with ad.
/// We assume that the collation information of the column is the same with the datum.
// go: Compare L1
pub fn compare(row: Row<'_>, col_idx: usize, ad: &Datum) -> isize {
    match ad.kind() {
        KindNull => {
            if row.is_null(col_idx) {
                return 0;
            }
            1
        }
        KindMinNotNull => {
            if row.is_null(col_idx) {
                return -1;
            }
            1
        }
        KindMaxValue => -1,
        KindInt64 => go_cmp(row.get_int64(col_idx), ad.get_int64()),
        KindUint64 => go_cmp(row.get_uint64(col_idx), ad.get_uint64()),
        KindFloat32 => go_cmp(row.get_float32(col_idx) as f64, ad.get_float32() as f64),
        KindFloat64 => go_cmp(row.get_float64(col_idx), ad.get_float64()),
        KindString => compare_string(row.get_string(col_idx), ad.get_string(), ad.collation()),
        KindBytes | KindBinaryLiteral | KindMysqlBit => {
            match row.get_bytes(col_idx).cmp(ad.get_bytes()) {
                Ordering::Less => -1,
                Ordering::Equal => 0,
                Ordering::Greater => 1,
            }
        }
        KindMysqlDecimal => {
            let (l, r) = (row.get_my_decimal(col_idx), ad.get_mysql_decimal());
            l.compare(r)
        }
        KindMysqlDuration => {
            let (l, r) = (
                row.get_duration(col_idx, 0).duration,
                ad.get_mysql_duration().duration,
            );
            go_cmp(l, r)
        }
        KindMysqlEnum => {
            let (l, r) = (row.get_enum(col_idx).value, ad.get_mysql_enum().value);
            go_cmp(l, r)
        }
        KindMysqlSet => {
            let (l, r) = (row.get_set(col_idx).value, ad.get_mysql_set().value);
            go_cmp(l, r)
        }
        KindMysqlJSON => {
            let (l, r) = (row.get_json(col_idx), ad.get_mysql_json());
            compare_binary_json(l, r)
        }
        KindVectorFloat32 => {
            let (l, r) = (row.get_vector_float32(col_idx), ad.get_vector_float32());
            l.compare(&r)
        }
        KindMysqlTime => {
            let (l, r) = (row.get_time(col_idx), ad.get_mysql_time());
            l.compare(&r)
        }
        _ => 0,
    }
}

/// Go's `sort.Search(n, f)`: the smallest index in `[0, n)` at which `f` is true, assuming
/// `f` is false then true; the calls are made in Go's order (`LowerBound`'s `f` has a side
/// effect).
// go-std: sort.Search L1
fn sort_search(n: usize, mut f: impl FnMut(usize) -> bool) -> usize {
    // Define f(-1) == false and f(n) == true.
    // Invariant: f(i-1) == false, f(j) == true.
    let (mut i, mut j) = (0usize, n);
    while i < j {
        let h = (i + j) >> 1; // avoid overflow when computing h
        // i ≤ h < j
        if !f(h) {
            i = h + 1; // preserves f(i-1) == false
        } else {
            j = h; // preserves f(j) == true
        }
    }
    // i == j, f(i-1) == false, and f(j) (= f(i)) == true  =>  answer is i.
    i
}

impl Chunk {
    /// LowerBound searches on the non-decreasing Column colIdx,
    /// returns the smallest index i such that the value at row i is not less than `d`.
    // go: Chunk.LowerBound L1
    pub fn lower_bound(&self, col_idx: usize, d: &Datum) -> (usize, bool) {
        // Go: c.GetRow(c.NumRows()-1) indexes row -1 of an empty chunk and panics.
        let last = self
            .num_rows()
            .checked_sub(1)
            .expect("index out of range [-1]");
        if compare(self.get_row(last), col_idx, d) < 0 {
            return (self.num_rows(), false);
        }
        let mut is_match = false;
        let index = sort_search(self.num_rows(), |i| {
            let cmp = compare(self.get_row(i), col_idx, d);
            if cmp == 0 {
                is_match = true;
            }
            cmp >= 0
        });
        (index, is_match)
    }

    /// UpperBound searches on the non-decreasing Column colIdx,
    /// returns the smallest index i such that the value at row i is larger than `d`.
    // go: Chunk.UpperBound L1
    pub fn upper_bound(&self, col_idx: usize, d: &Datum) -> usize {
        sort_search(self.num_rows(), |i| {
            compare(self.get_row(i), col_idx, d) > 0
        })
    }
}
