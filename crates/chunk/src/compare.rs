//! go-file: pkg/util/chunk/compare.go
//!
//! Row value comparison (N5a 3.1: L1). `Compare`, `Chunk.LowerBound` and `Chunk.UpperBound`
//! compare with a `types.Datum` (block D01.1) and wait for it. `types.CompareString` (D01.1)
//! is the one-line `collate.GetCollator(collation).Compare(x, y)`, called directly;
//! `types.BinaryLiteral.Compare` (D01.1) is reproduced in [`binary_literal_compare`] until
//! D01.1 lands.

use std::cmp::Ordering;

use tidb_base::mysql;
use tidb_parser::types::FieldType;
use tidb_types::collate;
use tidb_types::json::compare_binary_json;

use crate::row::Row;

/// CompareFunc is a function to compare the two values in Row, the two columns must have the same type.
pub type CompareFunc = Box<dyn Fn(Row<'_>, usize, Row<'_>, usize) -> isize + Send + Sync>;

/// GetCompareFunc gets a compare function for the field type.
// go: GetCompareFunc L1
pub fn get_compare_func(tp: &FieldType) -> Option<CompareFunc> {
    let f: CompareFunc = match tp.get_type() {
        mysql::TypeTiny | mysql::TypeShort | mysql::TypeInt24 | mysql::TypeLong | mysql::TypeLonglong | mysql::TypeYear => {
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
    Box::new(move |l, l_col, r, r_col| cmp_string_with_collation_info(l, l_col, r, r_col, &collation))
}

// go: cmpStringWithCollationInfo L1
fn cmp_string_with_collation_info(l: Row<'_>, l_col: usize, r: Row<'_>, r_col: usize, collation: &str) -> isize {
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
    let (l_dur, r_dur) = (l.get_duration(l_col, 0).duration, r.get_duration(r_col, 0).duration);
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
    binary_literal_compare(l.get_bytes(l_col), r.get_bytes(r_col))
}

/// `types.BinaryLiteral(b).Compare(types.BinaryLiteral(b2))`: compares after trimming
/// leading zero bytes, the longer value is larger.
// glue: types.BinaryLiteral.Compare and trimLeadingZeroBytes (block D01.1), until D01.1 lands
fn binary_literal_compare(b: &[u8], b2: &[u8]) -> isize {
    let trim = |x: &[u8]| -> usize {
        // trimLeadingZeroBytes keeps at least one byte.
        if x.is_empty() {
            return 0;
        }
        let mut pos = 0;
        let end = x.len() - 1;
        while pos < end && x[pos] == 0 {
            pos += 1;
        }
        pos
    };
    let (buf_b, buf_b2) = (&b[trim(b)..], &b2[trim(b2)..]);
    if buf_b.len() > buf_b2.len() {
        return 1;
    }
    if buf_b.len() < buf_b2.len() {
        return -1;
    }
    match buf_b.cmp(buf_b2) {
        Ordering::Less => -1,
        Ordering::Equal => 0,
        Ordering::Greater => 1,
    }
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
