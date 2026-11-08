//! go-file: pkg/types/datum_eval.go

#![allow(non_upper_case_globals)]

use tidb_parser::ast::Op;

use crate::datum::*;
use crate::errors::Error;
use crate::etc::inv_op2;
use crate::mydecimal::{MyDecimal, decimal_add};
use crate::overflow::{add_int64, add_integer, add_uint64};

/// ComputePlus computes the result of a+b.
// go: ComputePlus L1
#[allow(clippy::collapsible_match)] // Go's switch with an inner if
pub fn compute_plus(a: &Datum, b: &Datum) -> (Datum, Result<(), Error>) {
    let mut d = Datum::default();
    match a.kind() {
        KindInt64 => match b.kind() {
            KindInt64 => {
                let (r, err1) = split_zero(add_int64(a.get_int64(), b.get_int64()));
                d.set_int64(r);
                return (d, err1);
            }
            KindUint64 => {
                let (r, err1) = split_zero(add_integer(b.get_uint64(), a.get_int64()));
                d.set_uint64(r);
                return (d, err1);
            }
            _ => {}
        },
        KindUint64 => match b.kind() {
            KindInt64 => {
                let (r, err1) = split_zero(add_integer(a.get_uint64(), b.get_int64()));
                d.set_uint64(r);
                return (d, err1);
            }
            KindUint64 => {
                let (r, err1) = split_zero(add_uint64(a.get_uint64(), b.get_uint64()));
                d.set_uint64(r);
                return (d, err1);
            }
            _ => {}
        },
        KindFloat64 => {
            if b.kind() == KindFloat64 {
                let r = a.get_float64() + b.get_float64();
                d.set_float64(r);
                return (d, Ok(()));
            }
        }
        KindMysqlDecimal => {
            if b.kind() == KindMysqlDecimal {
                let mut r = MyDecimal::default();
                let err = decimal_add(a.get_mysql_decimal(), b.get_mysql_decimal(), &mut r);
                d.set_mysql_decimal(r);
                d.set_frac(a.frac().max(b.frac()));
                return (d, err);
            }
        }
        _ => {}
    }
    let err = inv_op2(&a.get_value(), &b.get_value(), Op::Plus);
    (d, Err(err))
}

/// Go's `(0, err)` of the overflow helpers.
// glue: Go's (T, error) pair of the overflow helpers
fn split_zero<T: Default>(r: Result<T, Error>) -> (T, Result<(), Error>) {
    match r {
        Ok(v) => (v, Ok(())),
        Err(e) => (T::default(), Err(e)),
    }
}
