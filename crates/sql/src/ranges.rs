//! Constants and handle ranges: turns `handle OP constant` conjunctions into one inclusive
//! range of integer handles.
//!
//! Comparisons are evaluated exactly, as Go's `RefineComparedConstant` + ranger do for an
//! integer column compared with a numeric constant: `id < 5.5` is `id <= 5`, `id = 5.5` is
//! empty, `id = '7'` is `id = 7`, any comparison with NULL is empty. Values beyond the `i64`
//! domain saturate (they cannot match any handle).

use tidb_mysql_protocol::SqlError;
use tidb_mysql_protocol::Value;

use crate::ir::CmpOp;
use crate::ir::Expr;
use crate::ir::Literal;

/// A numeric constant as `floor(value)` plus whether it has a non-zero fractional part.
/// `floor` saturates far outside the `i64` range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Num {
    pub floor: i128,
    pub frac: bool,
}

/// Saturation bound of [`Num::floor`] (well outside `i64`, far from `i128` overflow).
const SAT: i128 = 1 << 100;

impl Num {
    pub fn int(v: i128) -> Num {
        Num {
            floor: v.clamp(-SAT, SAT),
            frac: false,
        }
    }

    /// Parse a decimal number: `[+-]digits[.digits]` or `[+-].digits`. `None` if the text is
    /// not exactly that (exponents, spaces and trailing garbage are not accepted).
    pub fn parse_decimal(s: &str) -> Option<Num> {
        let (neg, body) = match s.as_bytes().first()? {
            b'-' => (true, &s[1..]),
            b'+' => (false, &s[1..]),
            _ => (false, s),
        };
        let (int_part, frac_part) = match body.split_once('.') {
            Some((i, f)) => (i, f),
            None => (body, ""),
        };
        if int_part.is_empty() && frac_part.is_empty() {
            return None;
        }
        if !int_part.bytes().all(|b| b.is_ascii_digit())
            || !frac_part.bytes().all(|b| b.is_ascii_digit())
        {
            return None;
        }
        let mut int: i128 = 0;
        for b in int_part.bytes() {
            int = (int * 10 + i128::from(b - b'0')).min(SAT);
        }
        let frac = frac_part.bytes().any(|b| b != b'0');
        Some(if neg {
            Num {
                floor: -int - i128::from(frac),
                frac,
            }
        } else {
            Num { floor: int, frac }
        })
    }

    /// `None` for NaN and infinities.
    pub fn from_f64(f: f64) -> Option<Num> {
        if !f.is_finite() {
            return None;
        }
        let fl = f.floor();
        Some(Num {
            floor: (fl as i128).clamp(-SAT, SAT),
            frac: fl != f,
        })
    }

    /// The value as an `i64` if it is an integer in range.
    pub fn as_i64(&self) -> Option<i64> {
        if self.frac {
            return None;
        }
        i64::try_from(self.floor).ok()
    }
}

/// A constant operand: NULL or a number. Non-numeric strings are rejected.
pub fn literal_num(lit: &Literal) -> Result<Option<Num>, SqlError> {
    Ok(Some(match lit {
        Literal::Null => return Ok(None),
        Literal::Bool(b) => Num::int(i128::from(*b)),
        Literal::Int(i) => Num::int(i128::from(*i)),
        Literal::UInt(u) => Num::int(i128::from(*u)),
        Literal::Decimal(s) => Num::parse_decimal(s)
            .ok_or_else(|| SqlError::not_supported(format!("decimal constant '{s}'")))?,
        Literal::Float(f) => Num::from_f64(*f)
            .ok_or_else(|| SqlError::not_supported(format!("float constant {f}")))?,
        Literal::Str(s) => Num::parse_decimal(s).ok_or_else(|| {
            SqlError::not_supported(format!(
                "comparing an integer column with the non-numeric string '{s}'"
            ))
        })?,
    }))
}

/// Converts a `COM_STMT_EXECUTE` parameter to a literal.
pub fn param_literal(v: &Value) -> Result<Literal, SqlError> {
    Ok(match v {
        Value::Null => Literal::Null,
        Value::Int(i) => Literal::Int(*i),
        Value::UInt(u) => Literal::UInt(*u),
        Value::Float(f) => Literal::Float(f64::from(*f)),
        Value::Double(d) => Literal::Float(*d),
        Value::Decimal(s) => Literal::Decimal(s.clone()),
        Value::Bytes(b) => Literal::Str(
            String::from_utf8(b.clone())
                .map_err(|_| SqlError::not_supported("non-UTF-8 string parameters"))?,
        ),
        Value::DateTime(_) | Value::Time(_) => {
            return Err(SqlError::not_supported("date/time parameters"));
        }
    })
}

/// Evaluate a constant expression (literal, `?`, unary minus of those).
pub fn eval_const(e: &Expr, params: &[Literal]) -> Result<Literal, SqlError> {
    match e {
        Expr::Literal(l) => Ok(l.clone()),
        Expr::Param(i) => params.get(*i).cloned().ok_or_else(|| {
            SqlError::new(
                1210,
                "Incorrect arguments to EXECUTE (parameter missing)".to_owned(),
            )
        }),
        Expr::Neg(inner) => negate(eval_const(inner, params)?),
        _ => Err(SqlError::not_supported("this expression as a constant")),
    }
}

fn negate(l: Literal) -> Result<Literal, SqlError> {
    Ok(match l {
        Literal::Null => Literal::Null,
        Literal::Bool(b) => Literal::Int(-i64::from(b)),
        Literal::Int(i) => match i.checked_neg() {
            Some(n) => Literal::Int(n),
            None => Literal::Decimal("9223372036854775808".to_owned()),
        },
        Literal::UInt(u) => {
            if u <= i64::MAX as u64 + 1 {
                Literal::Int((u as i64).wrapping_neg())
            } else {
                Literal::Decimal(format!("-{u}"))
            }
        }
        Literal::Decimal(s) => Literal::Decimal(match s.strip_prefix('-') {
            Some(rest) => rest.to_owned(),
            None => format!("-{s}"),
        }),
        Literal::Float(f) => Literal::Float(-f),
        Literal::Str(_) => return Err(SqlError::not_supported("negating a string")),
    })
}

/// An inclusive range of handles in `i128`, possibly empty.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Interval {
    pub lo: i128,
    pub hi: i128,
}

impl Interval {
    pub fn full() -> Interval {
        Interval {
            lo: i128::MIN,
            hi: i128::MAX,
        }
    }

    pub fn empty() -> Interval {
        Interval { lo: 1, hi: 0 }
    }

    pub fn is_empty(&self) -> bool {
        self.lo > self.hi
    }

    pub fn intersect(self, o: Interval) -> Interval {
        Interval {
            lo: self.lo.max(o.lo),
            hi: self.hi.min(o.hi),
        }
    }

    /// `handle OP value` (`None` value = NULL: never true).
    pub fn from_cmp(op: CmpOp, value: Option<Num>) -> Interval {
        let Some(v) = value else {
            return Interval::empty();
        };
        let (lo, hi) = (i128::MIN, i128::MAX);
        match op {
            CmpOp::Eq if v.frac => Interval::empty(),
            CmpOp::Eq => Interval {
                lo: v.floor,
                hi: v.floor,
            },
            // h < 5.5 <=> h <= 5;  h < 5 <=> h <= 4
            CmpOp::Lt => Interval {
                lo,
                hi: if v.frac { v.floor } else { v.floor - 1 },
            },
            CmpOp::Le => Interval { lo, hi: v.floor },
            CmpOp::Gt => Interval {
                lo: v.floor + 1,
                hi,
            },
            // h >= 5.5 <=> h >= 6
            CmpOp::Ge => Interval {
                lo: if v.frac { v.floor + 1 } else { v.floor },
                hi,
            },
        }
    }

    /// The range restricted to `i64`, or `None` if it holds no `i64`.
    pub fn to_i64(self) -> Option<(i64, i64)> {
        let lo = self.lo.max(i128::from(i64::MIN));
        let hi = self.hi.min(i128::from(i64::MAX));
        (lo <= hi).then_some((lo as i64, hi as i64))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(s: &str) -> Num {
        Num::parse_decimal(s).unwrap()
    }

    #[test]
    fn parse_numbers() {
        assert_eq!(
            n("12"),
            Num {
                floor: 12,
                frac: false
            }
        );
        assert_eq!(
            n("-12.50"),
            Num {
                floor: -13,
                frac: true
            }
        );
        assert_eq!(
            n("-12.000"),
            Num {
                floor: -12,
                frac: false
            }
        );
        assert_eq!(
            n(".5"),
            Num {
                floor: 0,
                frac: true
            }
        );
        assert_eq!(
            n("+7."),
            Num {
                floor: 7,
                frac: false
            }
        );
        assert_eq!(n("99999999999999999999999999999999999999999").floor, SAT);
        for bad in ["", "-", ".", "1e3", " 1", "1 ", "0x10", "1a", "--1"] {
            assert!(Num::parse_decimal(bad).is_none(), "{bad}");
        }
        assert_eq!(
            Num::from_f64(-0.5),
            Some(Num {
                floor: -1,
                frac: true
            })
        );
        assert_eq!(Num::from_f64(f64::NAN), None);
        assert_eq!(Num::from_f64(1e300).unwrap().floor, SAT);
    }

    #[test]
    fn comparisons() {
        let r = |op, s: &str| Interval::from_cmp(op, Some(n(s))).to_i64();
        assert_eq!(r(CmpOp::Eq, "5"), Some((5, 5)));
        assert_eq!(r(CmpOp::Eq, "5.5"), None);
        assert_eq!(r(CmpOp::Lt, "5"), Some((i64::MIN, 4)));
        assert_eq!(r(CmpOp::Lt, "5.5"), Some((i64::MIN, 5)));
        assert_eq!(r(CmpOp::Le, "5.5"), Some((i64::MIN, 5)));
        assert_eq!(r(CmpOp::Gt, "5.5"), Some((6, i64::MAX)));
        assert_eq!(r(CmpOp::Gt, "5"), Some((6, i64::MAX)));
        assert_eq!(r(CmpOp::Ge, "5.5"), Some((6, i64::MAX)));
        assert_eq!(r(CmpOp::Ge, "-5.5"), Some((-5, i64::MAX)));
        assert_eq!(r(CmpOp::Lt, "-5.5"), Some((i64::MIN, -6)));
        // Beyond i64.
        assert_eq!(r(CmpOp::Eq, "9223372036854775808"), None);
        assert_eq!(
            r(CmpOp::Le, "9223372036854775808"),
            Some((i64::MIN, i64::MAX))
        );
        assert_eq!(r(CmpOp::Gt, "9223372036854775807"), None);
        assert_eq!(r(CmpOp::Lt, "-9223372036854775808"), None);
        assert_eq!(
            r(CmpOp::Ge, "-99999999999999999999999"),
            Some((i64::MIN, i64::MAX))
        );
        // NULL never matches.
        assert_eq!(Interval::from_cmp(CmpOp::Ge, None).to_i64(), None);
        // BETWEEN 10 AND 5 is empty.
        let b = Interval::from_cmp(CmpOp::Ge, Some(n("10")))
            .intersect(Interval::from_cmp(CmpOp::Le, Some(n("5"))));
        assert!(b.is_empty());
        assert_eq!(b.to_i64(), None);
    }

    #[test]
    fn constants() {
        let p = [Literal::Int(5), Literal::UInt(u64::MAX)];
        assert_eq!(
            eval_const(&Expr::Neg(Box::new(Expr::Param(0))), &p).unwrap(),
            Literal::Int(-5)
        );
        assert_eq!(
            eval_const(&Expr::Neg(Box::new(Expr::Param(1))), &p).unwrap(),
            Literal::Decimal("-18446744073709551615".into())
        );
        assert_eq!(
            eval_const(
                &Expr::Neg(Box::new(Expr::Literal(Literal::UInt(1 << 63)))),
                &p
            )
            .unwrap(),
            Literal::Int(i64::MIN)
        );
        assert!(eval_const(&Expr::Param(2), &p).is_err());
        assert_eq!(
            literal_num(&Literal::Str("42".into())).unwrap(),
            Some(Num::int(42))
        );
        assert_eq!(
            literal_num(&Literal::Str("x".into())).unwrap_err().code,
            1235
        );
        assert_eq!(literal_num(&Literal::Null).unwrap(), None);
        assert_eq!(
            param_literal(&Value::Bytes(b"17".to_vec())).unwrap(),
            Literal::Str("17".into())
        );
    }
}
