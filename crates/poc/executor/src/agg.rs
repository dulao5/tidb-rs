//! Root hash aggregation: merges TiKV partial results (final mode) or aggregates scanned rows
//! (complete mode). Go: `executor/aggregate.HashAggExec` with `aggfuncs` (count, sum4Decimal,
//! sum4Float64, maxMin4*, firstRow4*).

use std::cmp::Ordering;
use std::collections::HashMap;

use tidb_distsql::Datum;
use tidb_meta::model::tp;
use tidb_mysql_protocol::SqlError;
use tidb_query_datatype::codec::mysql::Decimal;
use tidb_query_datatype::codec::mysql::decimal::RoundMode;
use tidb_sql::plan::AggKind;
use tidb_sql::plan::AggMode;
use tidb_sql::plan::RootAgg;
use tidb_sql::plan::RootAggFunc;

use crate::compare::StrCmp;
use crate::compare::compare;
use crate::compare::push_group_key;

/// `mysql.MaxDecimalScale`.
const MAX_DECIMAL_SCALE: i64 = 30;

#[derive(Clone, Debug)]
enum State {
    Count(i64),
    SumDec(Option<Decimal>),
    SumF64(Option<f64>),
    /// MIN/MAX: the best non-NULL value so far.
    Extreme(Option<Datum>),
    FirstRow(Option<Datum>),
}

struct Func<'a> {
    desc: &'a RootAggFunc,
    mode: AggMode,
    cmp: StrCmp,
}

impl Func<'_> {
    fn init(&self) -> State {
        match self.desc.kind {
            AggKind::Count => State::Count(0),
            AggKind::Sum if self.desc.ret_type.tp == tp::NEWDECIMAL => State::SumDec(None),
            AggKind::Sum => State::SumF64(None),
            AggKind::Min | AggKind::Max => State::Extreme(None),
            AggKind::FirstRow => State::FirstRow(None),
        }
    }

    fn update(&self, st: &mut State, d: &Datum) -> Result<(), SqlError> {
        match st {
            State::Count(n) => match (self.mode, d) {
                (_, Datum::Null) => {}
                (AggMode::Final, Datum::I64(c)) => *n += c,
                (AggMode::Final, Datum::U64(c)) => *n += *c as i64,
                (AggMode::Final, other) => return Err(bad_partial("count", other)),
                (AggMode::Complete, _) => *n += 1,
            },
            State::SumDec(acc) => {
                let v = match d {
                    Datum::Null => return Ok(()),
                    Datum::I64(i) => Decimal::from(*i),
                    Datum::U64(u) => Decimal::from(*u),
                    Datum::Dec(x) => *x,
                    other => return Err(bad_partial("sum", other)),
                };
                *acc = Some(match acc.take() {
                    None => v,
                    Some(a) => {
                        let r = &a + &v;
                        match r {
                            tidb_query_datatype::codec::mysql::Res::Ok(x) => x,
                            _ => {
                                return Err(SqlError::new(
                                    1690,
                                    "DECIMAL value is out of range in 'sum'",
                                ));
                            }
                        }
                    }
                });
            }
            State::SumF64(acc) => {
                let v = match d {
                    Datum::Null => return Ok(()),
                    Datum::F64(f) => *f,
                    Datum::I64(i) => *i as f64,
                    Datum::U64(u) => *u as f64,
                    other => return Err(bad_partial("sum", other)),
                };
                *acc = Some(acc.unwrap_or(0.0) + v);
            }
            State::Extreme(best) => {
                if matches!(d, Datum::Null) {
                    return Ok(());
                }
                let replace = match best {
                    None => true,
                    Some(b) => {
                        let ord = compare(d, b, self.cmp)?;
                        match self.desc.kind {
                            AggKind::Min => ord == Ordering::Less,
                            _ => ord == Ordering::Greater,
                        }
                    }
                };
                if replace {
                    *best = Some(d.clone());
                }
            }
            State::FirstRow(v) => {
                if v.is_none() {
                    *v = Some(d.clone());
                }
            }
        }
        Ok(())
    }

    fn finish(&self, st: State) -> Result<Datum, SqlError> {
        Ok(match st {
            State::Count(n) => Datum::I64(n),
            State::SumDec(None) | State::SumF64(None) => Datum::Null,
            State::SumDec(Some(d)) => {
                // Go `sum4Decimal.AppendFinalResult2Chunk`: round to the result scale.
                let mut frac = self.desc.ret_type.decimal;
                if frac < 0 {
                    frac = MAX_DECIMAL_SCALE;
                }
                let frac = frac.min(MAX_DECIMAL_SCALE) as i8;
                match d.round(frac, RoundMode::HalfEven) {
                    tidb_query_datatype::codec::mysql::Res::Ok(x) => Datum::Dec(x),
                    _ => {
                        return Err(SqlError::new(
                            1690,
                            "DECIMAL value is out of range in 'sum'",
                        ));
                    }
                }
            }
            State::SumF64(Some(f)) => Datum::F64(f),
            State::Extreme(v) | State::FirstRow(v) => v.unwrap_or(Datum::Null),
        })
    }
}

fn bad_partial(f: &str, d: &Datum) -> SqlError {
    SqlError::unknown(format!("unexpected {f} input {d:?}"))
}

/// Runs the aggregation; output rows hold one datum per function.
pub fn aggregate(
    agg: &RootAgg,
    rows: Vec<Vec<Datum>>,
    new_collation: bool,
) -> Result<Vec<Vec<Datum>>, SqlError> {
    let funcs: Vec<Func<'_>> = agg
        .funcs
        .iter()
        .map(|desc| {
            Ok(Func {
                desc,
                mode: agg.mode,
                cmp: StrCmp::of(&desc.arg_type, new_collation)?,
            })
        })
        .collect::<Result<_, SqlError>>()?;
    let group_cmp: Vec<StrCmp> = agg
        .group_types
        .iter()
        .map(|ft| StrCmp::of(ft, new_collation))
        .collect::<Result<_, _>>()?;

    // Groups in order of first appearance.
    let mut index: HashMap<Vec<u8>, usize> = HashMap::new();
    let mut groups: Vec<Vec<State>> = Vec::new();
    let mut key = Vec::new();
    for row in &rows {
        key.clear();
        for (&g, &c) in agg.group_by.iter().zip(&group_cmp) {
            push_group_key(&mut key, &row[g], c)?;
        }
        let gi = match index.get(&key) {
            Some(&i) => i,
            None => {
                groups.push(funcs.iter().map(Func::init).collect());
                index.insert(key.clone(), groups.len() - 1);
                groups.len() - 1
            }
        };
        for (f, st) in funcs.iter().zip(groups[gi].iter_mut()) {
            f.update(st, &row[f.desc.arg])?;
        }
    }
    // Without GROUP BY, an empty input still yields one row (COUNT 0, others NULL).
    if groups.is_empty() && agg.group_by.is_empty() {
        groups.push(funcs.iter().map(Func::init).collect());
    }
    groups
        .into_iter()
        .map(|states| {
            funcs
                .iter()
                .zip(states)
                .map(|(f, st)| f.finish(st))
                .collect()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use tidb_meta::model::FieldType;
    use tidb_meta::model::flag;
    use tidb_sql::plan::agg_ret_type;

    use super::*;

    fn int_ft() -> FieldType {
        FieldType {
            tp: tp::LONG,
            flag: flag::NOT_NULL,
            flen: 11,
            charset: "binary".into(),
            collate: "binary".into(),
            ..FieldType::default()
        }
    }

    fn str_ft() -> FieldType {
        FieldType {
            tp: tp::STRING,
            flen: 120,
            charset: "utf8mb4".into(),
            collate: "utf8mb4_bin".into(),
            ..FieldType::default()
        }
    }

    fn func(kind: AggKind, arg: usize, ft: FieldType, mode: AggMode) -> RootAggFunc {
        let ret = agg_ret_type(kind, &ft);
        RootAggFunc {
            kind,
            arg,
            arg_type: if mode == AggMode::Final {
                ret.clone()
            } else {
                ft
            },
            ret_type: ret,
        }
    }

    fn dec(s: &str) -> Datum {
        Datum::Dec(s.parse().unwrap())
    }

    fn bytes(s: &str) -> Datum {
        Datum::Bytes(s.as_bytes().to_vec())
    }

    #[test]
    fn final_sum_and_count_merge_partials() {
        let agg = RootAgg {
            mode: AggMode::Final,
            funcs: vec![
                func(AggKind::Sum, 0, int_ft(), AggMode::Final),
                func(AggKind::Count, 1, int_ft(), AggMode::Final),
            ],
            group_by: vec![],
            group_types: vec![],
        };
        let rows = vec![
            vec![dec("10"), Datum::I64(2)],
            vec![Datum::Null, Datum::I64(0)],
            vec![dec("-3"), Datum::I64(1)],
        ];
        let out = aggregate(&agg, rows, true).unwrap();
        assert_eq!(out, vec![vec![dec("7"), Datum::I64(3)]]);
        // No partials at all (empty range): one row, SUM NULL, COUNT 0.
        let out = aggregate(&agg, vec![], true).unwrap();
        assert_eq!(out, vec![vec![Datum::Null, Datum::I64(0)]]);
    }

    #[test]
    fn complete_mode_over_rows() {
        let agg = RootAgg {
            mode: AggMode::Complete,
            funcs: vec![
                func(AggKind::Sum, 0, int_ft(), AggMode::Complete),
                func(AggKind::Count, 0, int_ft(), AggMode::Complete),
                func(AggKind::Max, 1, str_ft(), AggMode::Complete),
                func(AggKind::Min, 0, int_ft(), AggMode::Complete),
            ],
            group_by: vec![],
            group_types: vec![],
        };
        let rows = vec![
            vec![Datum::I64(5), bytes("b")],
            vec![Datum::I64(-2), bytes("ab")],
        ];
        let out = aggregate(&agg, rows, true).unwrap();
        assert_eq!(
            out,
            vec![vec![dec("3"), Datum::I64(2), bytes("b"), Datum::I64(-2)]]
        );
    }

    #[test]
    fn group_by_with_collation() {
        // DISTINCT c: partial rows [firstrow(c), c].
        let agg = RootAgg {
            mode: AggMode::Final,
            funcs: vec![func(AggKind::FirstRow, 0, str_ft(), AggMode::Final)],
            group_by: vec![1],
            group_types: vec![str_ft()],
        };
        let rows = vec![
            vec![bytes("x"), bytes("x")],
            vec![bytes("y"), bytes("y")],
            vec![bytes("x "), bytes("x ")],
        ];
        let out = aggregate(&agg, rows, true).unwrap();
        assert_eq!(out, vec![vec![bytes("x")], vec![bytes("y")]]);
        // With GROUP BY, no input means no groups.
        assert!(aggregate(&agg, vec![], true).unwrap().is_empty());
    }
}
