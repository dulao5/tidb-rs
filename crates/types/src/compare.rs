//! go-file: pkg/types/compare.go

use std::cmp::Ordering;

use crate::collate;

/// `cmp.Compare` for integers.
// go-std: cmp.Compare L1
fn cmp_compare<T: Ord>(x: T, y: T) -> isize {
    match x.cmp(&y) {
        Ordering::Less => -1,
        Ordering::Equal => 0,
        Ordering::Greater => 1,
    }
}

/// VecCompareUU returns []int64 comparing the []uint64 x to []uint64 y
// go: VecCompareUU L1
pub fn vec_compare_uu(x: &[u64], y: &[u64], res: &mut [i64]) {
    let n = x.len();
    for i in 0..n {
        if x[i] < y[i] {
            res[i] = -1;
        } else if x[i] == y[i] {
            res[i] = 0;
        } else {
            res[i] = 1;
        }
    }
}

/// VecCompareII returns []int64 comparing the []int64 x to []int64 y
// go: VecCompareII L1
pub fn vec_compare_ii(x: &[i64], y: &[i64], res: &mut [i64]) {
    let n = x.len();
    for i in 0..n {
        if x[i] < y[i] {
            res[i] = -1;
        } else if x[i] == y[i] {
            res[i] = 0;
        } else {
            res[i] = 1;
        }
    }
}

/// VecCompareUI returns []int64 comparing the []uint64 x to []int64y
// go: VecCompareUI L1
#[allow(clippy::if_same_then_else)] // Go's branch structure
pub fn vec_compare_ui(x: &[u64], y: &[i64], res: &mut [i64]) {
    let n = x.len();
    for i in 0..n {
        if y[i] < 0 || x[i] > i64::MAX as u64 {
            res[i] = 1;
        } else if (x[i] as i64) < y[i] {
            res[i] = -1;
        } else if x[i] as i64 == y[i] {
            res[i] = 0;
        } else {
            res[i] = 1;
        }
    }
}

/// VecCompareIU returns []int64 comparing the []int64 x to []uint64y
// go: VecCompareIU L1
#[allow(clippy::if_same_then_else)] // Go's branch structure
pub fn vec_compare_iu(x: &[i64], y: &[u64], res: &mut [i64]) {
    let n = x.len();
    for i in 0..n {
        if x[i] < 0 || y[i] > i64::MAX as u64 {
            res[i] = -1;
        } else if x[i] < y[i] as i64 {
            res[i] = -1;
        } else if x[i] == y[i] as i64 {
            res[i] = 0;
        } else {
            res[i] = 1;
        }
    }
}

/// CompareString returns an integer comparing the string x to y with the specified collation and length.
// go: CompareString L1
pub fn compare_string(x: &[u8], y: &[u8], collation: &str) -> isize {
    collate::get_collator(collation).compare(x, y)
}

/// CompareInt return an integer comparing the integer x to y with signed or unsigned.
// go: CompareInt L1
pub fn compare_int(arg0: i64, is_unsigned0: bool, arg1: i64, is_unsigned1: bool) -> isize {
    match (is_unsigned0, is_unsigned1) {
        (true, true) => cmp_compare(arg0 as u64, arg1 as u64),
        (true, false) => {
            if arg1 < 0 || arg0 as u64 > i64::MAX as u64 {
                1
            } else {
                cmp_compare(arg0, arg1)
            }
        }
        (false, true) => {
            if arg0 < 0 || arg1 as u64 > i64::MAX as u64 {
                -1
            } else {
                cmp_compare(arg0, arg1)
            }
        }
        (false, false) => cmp_compare(arg0, arg1),
    }
}
