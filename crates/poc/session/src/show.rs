//! `SHOW [GLOBAL | SESSION] VARIABLES [LIKE ... | WHERE ...]` over live `SessionVars`
//! and cluster globals (Go: `ShowExec.fetchShowVariables`, `ShowExec.Next`, `buildShowSchema`).

#![allow(non_upper_case_globals)]

use std::sync::atomic::Ordering::SeqCst;

use tidb_base::contextutil::SQLWarn;
use tidb_base::mysql::DefaultCollationName;
use tidb_mysql_protocol::constants::collation;
use tidb_mysql_protocol::{ColumnDef, ColumnType, ResultSet, SqlError, Value};
use tidb_sessionctx::variable::session::SessionVars;
use tidb_sessionctx::variable::tidb_vars::EnableNoopVariables;
use tidb_sessionctx::variable::{ScopeSession, get_sys_vars};
use tidb_sql::ir::{Literal, ShowBinaryOp, ShowExpr, ShowUnaryOp};
use tidb_types::context::DefaultStmtNoWarningContext;
use tidb_types::datum::{
    Datum, KindFloat64, KindInt64, KindMysqlDecimal, KindNull, KindString, KindUint64,
};
use tidb_types::mydecimal::{MyDecimal, decimal_neg};
use tidb_types::stringutil::{compile_pattern, do_match};

use crate::constant::read_sysvar_string;

/// Column metadata for `SHOW [GLOBAL|SESSION] VARIABLES` before row-length expansion (`buildShowSchema`).
pub(crate) fn show_variables_columns() -> [ColumnDef; 2] {
    let make = |name: &str| {
        let mut col = ColumnDef::new(name, ColumnType::VarString)
            .with_length(20)
            .with_decimals(0);
        col.org_name.clear();
        col.charset = collation::UTF8MB4_BIN;
        col
    };
    [make("Variable_name"), make("Value")]
}

/// Evaluate `SHOW [GLOBAL|SESSION] VARIABLES [LIKE pattern | WHERE expr]`.
// glue: Go's ShowExec.fetchShowVariables + ShowExec.Next length expansion + Selection(WHERE)
#[allow(clippy::too_many_arguments)]
pub(crate) fn show_variables(
    vars: &mut SessionVars,
    global: bool,
    like: Option<&ShowExpr>,
    where_: Option<&ShowExpr>,
    params: &[Literal],
    server_version: &str,
    have_ssl: &str,
    warnings: &mut Vec<SQLWarn>,
) -> Result<ResultSet, SqlError> {
    if let Some(w) = where_ {
        validate_where_columns(w)?;
    }

    let pattern = match like {
        Some(expr) => {
            let d = eval_expr(expr, None, vars, params, server_version, have_ssl, warnings)?;
            if d.is_null() {
                None
            } else {
                let s = datum_to_string(&d)?.to_lowercase();
                Some(compile_pattern(s.as_bytes(), b'\\'))
            }
        }
        None => None,
    };

    let [mut name_col, mut val_col] = show_variables_columns();
    if like.is_some() && pattern.is_none() {
        return Ok(ResultSet {
            columns: vec![name_col, val_col],
            rows: Vec::new(),
            warnings: 0,
        });
    }

    let enable_noop = EnableNoopVariables.load(SeqCst);
    let mut svs: Vec<_> = get_sys_vars()
        .filter(|sv| {
            if global && sv.scope == ScopeSession {
                return false;
            }
            if sv.is_noop && !enable_noop {
                return false;
            }
            if let Some((pat_chars, pat_types)) = &pattern
                && !do_match(sv.name.as_bytes(), pat_chars, pat_types)
            {
                return false;
            }
            true
        })
        .collect();
    svs.sort_by_key(|sv| sv.name);

    let mut pairs = Vec::with_capacity(svs.len());
    for sv in svs {
        let val = read_sysvar_string(vars, &sv, server_version, have_ssl, global)?;
        pairs.push((sv.name.to_owned(), val));
    }

    // Go's ShowExec.Next expands VARCHAR RetType flen over all fetched rows before Selection (WHERE).
    let max_name = pairs.iter().map(|(n, _)| n.len()).max().unwrap_or(0).max(5) as u32;
    let max_val = pairs.iter().map(|(_, v)| v.len()).max().unwrap_or(0).max(5) as u32;
    name_col.column_length = max_name.saturating_mul(4);
    val_col.column_length = max_val.saturating_mul(4);

    let mut rows = Vec::new();
    for (name, val) in pairs {
        if let Some(w) = where_ {
            let d = eval_expr(
                w,
                Some((&name, &val)),
                vars,
                params,
                server_version,
                have_ssl,
                warnings,
            )?;
            if !datum_is_true(&d)? {
                continue;
            }
        }
        rows.push(vec![Value::from(name), Value::from(val)]);
    }

    Ok(ResultSet {
        columns: vec![name_col, val_col],
        rows,
        warnings: 0,
    })
}

pub(crate) fn validate_where_columns(e: &ShowExpr) -> Result<(), SqlError> {
    match e {
        ShowExpr::Column(c) => {
            let valid = c.schema.is_empty()
                && c.table.is_empty()
                && (c.name.eq_ignore_ascii_case("variable_name")
                    || c.name.eq_ignore_ascii_case("value"));
            if !valid {
                return Err(SqlError::new(
                    1054,
                    format!("Unknown column '{}' in 'where clause'", c.orig_col_name()),
                ));
            }
            Ok(())
        }
        ShowExpr::Unary { expr, .. }
        | ShowExpr::IsNull { expr, .. }
        | ShowExpr::IsTruth { expr, .. } => validate_where_columns(expr),
        ShowExpr::Binary { left, right, .. } => {
            validate_where_columns(left)?;
            validate_where_columns(right)
        }
        ShowExpr::Like { expr, pattern, .. } => {
            validate_where_columns(expr)?;
            validate_where_columns(pattern)
        }
        ShowExpr::In { expr, list, .. } => {
            validate_where_columns(expr)?;
            for item in list {
                validate_where_columns(item)?;
            }
            Ok(())
        }
        ShowExpr::Between {
            expr, low, high, ..
        } => {
            validate_where_columns(expr)?;
            validate_where_columns(low)?;
            validate_where_columns(high)
        }
        ShowExpr::Literal(_) | ShowExpr::Param(_) | ShowExpr::SysVar { .. } => Ok(()),
    }
}

fn eval_expr(
    e: &ShowExpr,
    row: Option<(&str, &str)>,
    vars: &mut SessionVars,
    params: &[Literal],
    server_version: &str,
    have_ssl: &str,
    warnings: &mut Vec<SQLWarn>,
) -> Result<Datum, SqlError> {
    match e {
        ShowExpr::Column(c) => {
            let Some((var_name, var_val)) = row else {
                return Err(SqlError::new(
                    1054,
                    format!("Unknown column '{}' in 'field list'", c.orig_col_name()),
                ));
            };
            let s = if c.name.eq_ignore_ascii_case("variable_name") {
                var_name
            } else {
                var_val
            };
            Ok(tidb_types::datum::new_collation_string_datum(
                s,
                DefaultCollationName,
            ))
        }
        ShowExpr::Literal(lit) => Ok(literal_to_datum(lit)),
        ShowExpr::Param(idx) => {
            let lit = params.get(*idx).ok_or_else(|| {
                SqlError::unknown(format!("missing parameter {idx} for SHOW VARIABLES"))
            })?;
            Ok(literal_to_datum(lit))
        }
        ShowExpr::SysVar {
            name,
            global,
            explicit_scope,
        } => crate::constant::eval_sysvar_datum(
            vars,
            name,
            *global,
            *explicit_scope,
            server_version,
            have_ssl,
            warnings,
        ),
        ShowExpr::Unary { op, expr } => {
            let d = eval_expr(expr, row, vars, params, server_version, have_ssl, warnings)?;
            match op {
                ShowUnaryOp::Plus => Ok(d),
                ShowUnaryOp::Not => {
                    if d.is_null() {
                        Ok(Datum::default())
                    } else {
                        Ok(tidb_types::datum::new_int_datum(i64::from(!datum_is_true(
                            &d,
                        )?)))
                    }
                }
                ShowUnaryOp::Minus => negate_datum(&d),
            }
        }
        ShowExpr::Binary { op, left, right } => match op {
            ShowBinaryOp::And => {
                let l = eval_expr(left, row, vars, params, server_version, have_ssl, warnings)?;
                if !l.is_null() && !datum_is_true(&l)? {
                    return Ok(tidb_types::datum::new_int_datum(0));
                }
                let r = eval_expr(right, row, vars, params, server_version, have_ssl, warnings)?;
                if !r.is_null() && !datum_is_true(&r)? {
                    return Ok(tidb_types::datum::new_int_datum(0));
                }
                if l.is_null() || r.is_null() {
                    Ok(Datum::default())
                } else {
                    Ok(tidb_types::datum::new_int_datum(1))
                }
            }
            ShowBinaryOp::Or => {
                let l = eval_expr(left, row, vars, params, server_version, have_ssl, warnings)?;
                if !l.is_null() && datum_is_true(&l)? {
                    return Ok(tidb_types::datum::new_int_datum(1));
                }
                let r = eval_expr(right, row, vars, params, server_version, have_ssl, warnings)?;
                if !r.is_null() && datum_is_true(&r)? {
                    return Ok(tidb_types::datum::new_int_datum(1));
                }
                if l.is_null() || r.is_null() {
                    Ok(Datum::default())
                } else {
                    Ok(tidb_types::datum::new_int_datum(0))
                }
            }
            ShowBinaryOp::NullEq => {
                let l = eval_expr(left, row, vars, params, server_version, have_ssl, warnings)?;
                let r = eval_expr(right, row, vars, params, server_version, have_ssl, warnings)?;
                if l.is_null() && r.is_null() {
                    return Ok(tidb_types::datum::new_int_datum(1));
                }
                if l.is_null() || r.is_null() {
                    return Ok(tidb_types::datum::new_int_datum(0));
                }
                Ok(tidb_types::datum::new_int_datum(i64::from(
                    compare_datums(&l, &r) == 0,
                )))
            }
            ShowBinaryOp::Eq
            | ShowBinaryOp::Ne
            | ShowBinaryOp::Lt
            | ShowBinaryOp::Le
            | ShowBinaryOp::Gt
            | ShowBinaryOp::Ge => {
                let l = eval_expr(left, row, vars, params, server_version, have_ssl, warnings)?;
                let r = eval_expr(right, row, vars, params, server_version, have_ssl, warnings)?;
                if l.is_null() || r.is_null() {
                    return Ok(Datum::default());
                }
                let ord = compare_datums(&l, &r);
                let res = match op {
                    ShowBinaryOp::Eq => ord == 0,
                    ShowBinaryOp::Ne => ord != 0,
                    ShowBinaryOp::Lt => ord < 0,
                    ShowBinaryOp::Le => ord <= 0,
                    ShowBinaryOp::Gt => ord > 0,
                    ShowBinaryOp::Ge => ord >= 0,
                    _ => unreachable!(),
                };
                Ok(tidb_types::datum::new_int_datum(i64::from(res)))
            }
        },
        ShowExpr::Like {
            not,
            expr,
            pattern,
            escape,
        } => {
            let l = eval_expr(expr, row, vars, params, server_version, have_ssl, warnings)?;
            let p = eval_expr(
                pattern,
                row,
                vars,
                params,
                server_version,
                have_ssl,
                warnings,
            )?;
            if l.is_null() || p.is_null() {
                return Ok(Datum::default());
            }
            let ls = datum_to_string(&l)?;
            let ps = datum_to_string(&p)?;
            // `Variable_name` and `Value` carry `utf8mb4_bin` collation (case-sensitive match in WHERE).
            let (pat_chars, pat_types) = compile_pattern(ps.as_bytes(), *escape);
            let matched = do_match(ls.as_bytes(), &pat_chars, &pat_types);
            Ok(tidb_types::datum::new_int_datum(i64::from(matched ^ *not)))
        }
        ShowExpr::In { not, expr, list } => {
            let lhs = eval_expr(expr, row, vars, params, server_version, have_ssl, warnings)?;
            if lhs.is_null() {
                return Ok(Datum::default());
            }
            let mut has_null = false;
            for item in list {
                let rhs = eval_expr(item, row, vars, params, server_version, have_ssl, warnings)?;
                if rhs.is_null() {
                    has_null = true;
                    continue;
                }
                if compare_datums(&lhs, &rhs) == 0 {
                    return Ok(tidb_types::datum::new_int_datum(i64::from(!*not)));
                }
            }
            if has_null {
                Ok(Datum::default())
            } else {
                Ok(tidb_types::datum::new_int_datum(i64::from(*not)))
            }
        }
        ShowExpr::Between {
            not,
            expr,
            low,
            high,
        } => {
            let v = eval_expr(expr, row, vars, params, server_version, have_ssl, warnings)?;
            let lo = eval_expr(low, row, vars, params, server_version, have_ssl, warnings)?;
            let hi = eval_expr(high, row, vars, params, server_version, have_ssl, warnings)?;
            if v.is_null() || lo.is_null() || hi.is_null() {
                return Ok(Datum::default());
            }
            let in_range = compare_datums(&v, &lo) >= 0 && compare_datums(&v, &hi) <= 0;
            Ok(tidb_types::datum::new_int_datum(i64::from(in_range ^ *not)))
        }
        ShowExpr::IsNull { not, expr } => {
            let d = eval_expr(expr, row, vars, params, server_version, have_ssl, warnings)?;
            Ok(tidb_types::datum::new_int_datum(i64::from(
                d.is_null() ^ *not,
            )))
        }
        ShowExpr::IsTruth {
            not,
            true_val,
            expr,
        } => {
            let d = eval_expr(expr, row, vars, params, server_version, have_ssl, warnings)?;
            let res = if d.is_null() {
                false
            } else {
                datum_is_true(&d)? == *true_val
            };
            Ok(tidb_types::datum::new_int_datum(i64::from(res ^ *not)))
        }
    }
}

fn literal_to_datum(lit: &Literal) -> Datum {
    match lit {
        Literal::Null => Datum::default(),
        Literal::Bool(b) => tidb_types::datum::new_int_datum(i64::from(*b)),
        Literal::Int(i) => tidb_types::datum::new_int_datum(*i),
        Literal::UInt(u) => tidb_types::datum::new_uint_datum(*u),
        Literal::Float(f) => tidb_types::datum::new_float64_datum(*f),
        Literal::Decimal(s) => {
            let mut dec = MyDecimal::default();
            let _ = dec.from_string(s.as_bytes());
            tidb_types::datum::new_decimal_datum(dec)
        }
        Literal::Str(s) => {
            tidb_types::datum::new_collation_string_datum(s.as_str(), DefaultCollationName)
        }
    }
}

fn negate_datum(d: &Datum) -> Result<Datum, SqlError> {
    if d.is_null() {
        return Ok(Datum::default());
    }
    match d.kind() {
        KindInt64 => Ok(tidb_types::datum::new_int_datum(
            d.get_int64().wrapping_neg(),
        )),
        KindUint64 => {
            let u = d.get_uint64();
            if u <= i64::MAX as u64 + 1 {
                Ok(tidb_types::datum::new_int_datum(-(u as i128) as i64))
            } else {
                let mut dec = MyDecimal::default();
                dec.from_uint(u);
                Ok(tidb_types::datum::new_decimal_datum(decimal_neg(&dec)))
            }
        }
        KindFloat64 => Ok(tidb_types::datum::new_float64_datum(-d.get_float64())),
        KindMysqlDecimal => Ok(tidb_types::datum::new_decimal_datum(decimal_neg(
            d.get_mysql_decimal(),
        ))),
        _ => {
            let s = datum_to_string(d)?;
            let f = tidb_types::convert::str_to_float(
                &DefaultStmtNoWarningContext,
                s.as_bytes(),
                false,
            )
            .0;
            Ok(tidb_types::datum::new_float64_datum(-f))
        }
    }
}

fn datum_is_true(d: &Datum) -> Result<bool, SqlError> {
    if d.is_null() {
        return Ok(false);
    }
    Ok(match d.kind() {
        KindInt64 => d.get_int64() != 0,
        KindUint64 => d.get_uint64() != 0,
        KindFloat64 => d.get_float64() != 0.0,
        KindMysqlDecimal => !d.get_mysql_decimal().is_zero(),
        _ => {
            let s = datum_to_string(d)?;
            let f = tidb_types::convert::str_to_float(
                &DefaultStmtNoWarningContext,
                s.as_bytes(),
                false,
            )
            .0;
            f != 0.0
        }
    })
}

fn datum_to_string(d: &Datum) -> Result<String, SqlError> {
    String::from_utf8(d.to_string().map_err(|e| crate::router::sql_error(&e))?)
        .map_err(|e| SqlError::unknown(e.to_string()))
}

fn compare_datums(l: &Datum, r: &Datum) -> i8 {
    if l.kind() == KindString && r.kind() == KindString {
        return match l.get_string().cmp(r.get_string()) {
            std::cmp::Ordering::Less => -1,
            std::cmp::Ordering::Equal => 0,
            std::cmp::Ordering::Greater => 1,
        };
    }
    if (l.kind() == KindInt64 || l.kind() == KindUint64)
        && (r.kind() == KindInt64 || r.kind() == KindUint64)
    {
        let lv: i128 = if l.kind() == KindInt64 {
            i128::from(l.get_int64())
        } else {
            i128::from(l.get_uint64())
        };
        let rv: i128 = if r.kind() == KindInt64 {
            i128::from(r.get_int64())
        } else {
            i128::from(r.get_uint64())
        };
        return match lv.cmp(&rv) {
            std::cmp::Ordering::Less => -1,
            std::cmp::Ordering::Equal => 0,
            std::cmp::Ordering::Greater => 1,
        };
    }
    let to_f64 = |d: &Datum| match d.kind() {
        KindInt64 => d.get_int64() as f64,
        KindUint64 => d.get_uint64() as f64,
        KindFloat64 => d.get_float64(),
        KindMysqlDecimal => {
            let (f, _) = d.get_mysql_decimal().to_float64();
            f
        }
        KindNull => 0.0,
        _ => {
            let b = d.to_string().unwrap_or_default();
            tidb_types::convert::str_to_float(&DefaultStmtNoWarningContext, &b, false).0
        }
    };
    let lf = to_f64(l);
    let rf = to_f64(r);
    match lf.partial_cmp(&rf) {
        Some(std::cmp::Ordering::Less) => -1,
        Some(std::cmp::Ordering::Equal) => 0,
        Some(std::cmp::Ordering::Greater) => 1,
        None => 0,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use parking_lot::RwLock;
    use tidb_session::{Dialect, Session};
    use tidb_systable::sysvar_cache::SysVarCache;

    use super::*;
    use crate::router::newstack::ClusterGlobals;

    fn session_over(rows: &[(&str, &str)], tidb_rows: &[(&str, &str)]) -> Session {
        let globals = if rows.is_empty() && tidb_rows.is_empty() {
            ClusterGlobals::compiled_defaults()
        } else {
            let table: HashMap<String, String> = rows
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect();
            let cache = SysVarCache::new();
            cache.rebuild(None, &mut || Ok(table.clone())).unwrap();
            let tidb_map: HashMap<String, String> = tidb_rows
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect();
            ClusterGlobals::new(Arc::new(cache)).with_tidb_table(Arc::new(RwLock::new(tidb_map)))
        };
        let mut s = Session::new(Dialect::MySQL, Box::new(globals));
        s.load_common_global_variables_if_needed().unwrap();
        s
    }

    fn run_show(
        s: &mut Session,
        sql: &str,
        params: &[Literal],
        have_ssl: &str,
    ) -> Result<ResultSet, SqlError> {
        let l = tidb_sql::parse_and_lower(sql)?;
        let tidb_sql::Statement::ShowVariables {
            global,
            like,
            where_,
        } = l.stmt
        else {
            panic!("{sql}");
        };
        let mut warnings = Vec::new();
        show_variables(
            s.vars_mut(),
            global,
            like.as_ref(),
            where_.as_ref(),
            params,
            "8.0.11-TiDB-tidb-rs",
            have_ssl,
            &mut warnings,
        )
    }

    fn kv_rows(rs: &ResultSet) -> Vec<(String, String)> {
        rs.rows
            .iter()
            .map(|r| match &r[..] {
                [Value::Bytes(k), Value::Bytes(v)] => (
                    String::from_utf8_lossy(k).into_owned(),
                    String::from_utf8_lossy(v).into_owned(),
                ),
                other => panic!("unexpected row {other:?}"),
            })
            .collect()
    }

    #[test]
    fn show_variables_global_vs_session_counts_and_sort() {
        let mut s = session_over(&[], &[]);
        let sess = run_show(&mut s, "SHOW VARIABLES", &[], "DISABLED").unwrap();
        let glob = run_show(&mut s, "SHOW GLOBAL VARIABLES", &[], "DISABLED").unwrap();
        let sess_kv = kv_rows(&sess);
        let glob_kv = kv_rows(&glob);
        assert!(sess_kv.len() > glob_kv.len());
        assert!(sess_kv.iter().any(|(k, _)| k == "warning_count"));
        assert!(!glob_kv.iter().any(|(k, _)| k == "warning_count"));
        assert!(sess_kv.windows(2).all(|w| w[0].0 < w[1].0));
        assert!(glob_kv.windows(2).all(|w| w[0].0 < w[1].0));
        assert_eq!(sess.columns[0].name, "Variable_name");
        assert_eq!(sess.columns[1].name, "Value");
        assert_eq!(sess.columns[0].charset, collation::UTF8MB4_BIN);
        assert_eq!(sess.columns[1].charset, collation::UTF8MB4_BIN);
    }

    #[test]
    fn show_variables_like_and_where_semantics() {
        let mut s = session_over(&[], &[]);
        let all = run_show(&mut s, "SHOW VARIABLES", &[], "YES").unwrap();
        let full_name_len = all.columns[0].column_length;
        let full_val_len = all.columns[1].column_length;

        // LIKE is case-insensitive and expands column_length only across LIKE-matched rows.
        let like_auto = run_show(&mut s, "SHOW VARIABLES LIKE 'AUTO%'", &[], "YES").unwrap();
        let rows_auto = kv_rows(&like_auto);
        assert!(rows_auto.iter().any(|(k, _)| k == "autocommit"));
        let max_name = rows_auto.iter().map(|(k, _)| k.len()).max().unwrap().max(5) as u32;
        assert_eq!(like_auto.columns[0].column_length, max_name * 4);

        // 0-row LIKE keeps default length 20 (`max(5, 0) * 4`).
        let empty_like = run_show(&mut s, "SHOW VARIABLES LIKE 'no_such_%'", &[], "YES").unwrap();
        assert!(empty_like.rows.is_empty());
        assert_eq!(empty_like.columns[0].column_length, 20);
        assert_eq!(empty_like.columns[1].column_length, 20);

        let null_like = run_show(&mut s, "SHOW VARIABLES LIKE NULL", &[], "YES").unwrap();
        assert!(null_like.rows.is_empty());
        assert_eq!(null_like.columns[0].column_length, 20);

        // WHERE is case-sensitive (utf8mb4_bin) and runs after ShowExec column_length expansion.
        let wrong_case = run_show(
            &mut s,
            "SHOW VARIABLES WHERE Variable_name = 'Sql_mode'",
            &[],
            "YES",
        )
        .unwrap();
        assert!(wrong_case.rows.is_empty());
        assert_eq!(wrong_case.columns[0].column_length, full_name_len);
        assert_eq!(wrong_case.columns[1].column_length, full_val_len);

        let exact_case = run_show(
            &mut s,
            "SHOW VARIABLES WHERE Variable_name = 'sql_mode' AND Value = @@sql_mode",
            &[],
            "YES",
        )
        .unwrap();
        assert_eq!(kv_rows(&exact_case).len(), 1);

        // Complex WHERE predicates + prepared param in LIKE.
        let multi = run_show(
            &mut s,
            "SHOW VARIABLES WHERE Variable_name IN ('div_precision_increment', 'have_ssl') AND Value IS NOT NULL",
            &[],
            "YES",
        )
        .unwrap();
        assert_eq!(
            kv_rows(&multi),
            vec![
                ("div_precision_increment".to_owned(), "4".to_owned()),
                ("have_ssl".to_owned(), "YES".to_owned()),
            ]
        );
        let like_param = run_show(
            &mut s,
            "SHOW VARIABLES LIKE ?",
            &[Literal::Str("max_execution_time".into())],
            "YES",
        )
        .unwrap();
        assert_eq!(
            kv_rows(&like_param),
            vec![("max_execution_time".to_owned(), "0".to_owned())]
        );

        // Column & sysvar errors in WHERE.
        let e = run_show(&mut s, "SHOW VARIABLES WHERE bad_col = 1", &[], "YES").unwrap_err();
        assert_eq!(
            (e.code, e.message.as_str()),
            (1054, "Unknown column 'bad_col' in 'where clause'")
        );
        let e = run_show(
            &mut s,
            "SHOW VARIABLES WHERE t.Variable_name = 'sql_mode'",
            &[],
            "YES",
        )
        .unwrap_err();
        assert_eq!(e.code, 1054);
        let e = run_show(
            &mut s,
            "SHOW VARIABLES WHERE Variable_name = @@no_such_var",
            &[],
            "YES",
        )
        .unwrap_err();
        assert_eq!(e.code, 1193);
    }

    #[test]
    fn show_variables_reads_cluster_globals_and_tidb_table() {
        let mut s = session_over(
            &[
                ("sql_mode", "ANSI_QUOTES"),
                ("time_zone", "+08:00"),
                ("div_precision_increment", "6"),
                ("group_concat_max_len", "2048"),
                ("max_execution_time", "1500"),
                ("tidb_enable_noop_functions", "ON"),
                ("tidb_skip_isolation_level_check", "ON"),
                ("tidb_enable_prepared_plan_cache", "OFF"),
            ],
            &[("tikv_gc_life_time", "24h0m0s")],
        );
        let glob = run_show(
            &mut s,
            "SHOW GLOBAL VARIABLES WHERE Variable_name IN ('sql_mode', 'time_zone', 'div_precision_increment', 'group_concat_max_len', 'max_execution_time', 'tidb_enable_noop_functions', 'tidb_skip_isolation_level_check', 'tidb_enable_prepared_plan_cache', 'tidb_gc_life_time')",
            &[],
            "DISABLED",
        )
        .unwrap();
        assert_eq!(
            kv_rows(&glob),
            vec![
                ("div_precision_increment".to_owned(), "6".to_owned()),
                ("group_concat_max_len".to_owned(), "2048".to_owned()),
                ("max_execution_time".to_owned(), "1500".to_owned()),
                ("sql_mode".to_owned(), "ANSI_QUOTES".to_owned()),
                ("tidb_enable_noop_functions".to_owned(), "ON".to_owned()),
                (
                    "tidb_enable_prepared_plan_cache".to_owned(),
                    "OFF".to_owned()
                ),
                ("tidb_gc_life_time".to_owned(), "24h0m0s".to_owned()),
                (
                    "tidb_skip_isolation_level_check".to_owned(),
                    "ON".to_owned()
                ),
                ("time_zone".to_owned(), "+08:00".to_owned()),
            ]
        );
    }
}
