//! SELECTs without FROM: literals, `?`, system variables and a few session functions. This is
//! what the `mysql` CLI and connectors send while connecting (`SELECT @@version_comment
//! LIMIT 1`, `SELECT DATABASE(), USER()`, ...).

use tidb_mysql_protocol::ColumnDef;
use tidb_mysql_protocol::ColumnType;
use tidb_mysql_protocol::ResultSet;
use tidb_mysql_protocol::SqlError;
use tidb_mysql_protocol::Value;
use tidb_mysql_protocol::constants::collation;
use tidb_mysql_protocol::constants::column_flag;
use tidb_sql::ir::Expr;
use tidb_sql::ir::Field;
use tidb_sql::ir::Literal;
use tidb_sql::ir::Select;
use tidb_sql::ranges::eval_const;

use crate::router::sql_error;
use tidb_base::contextutil::{SQLWarn, WarnLevelWarning};
use tidb_base::dbterror::plannererrors::ErrGettingNoopVariable;
use tidb_base::go_args;
use tidb_sessionctx::variable::SysVar;
use tidb_sessionctx::variable::session::SessionVars;
use tidb_types::datum::Datum;

/// Session values the constant evaluator reads.
pub struct ConstEnv<'a> {
    pub server_version: &'a str,
    pub current_db: Option<&'a str>,
    /// `USER()`: `user@client-host`.
    pub user: &'a str,
    /// `CURRENT_USER()`: the matched account.
    pub current_user: &'a str,
    pub connection_id: u64,
    pub have_ssl: &'a str,
    /// The prototype's session variables (`Session::default_session`): the session values its
    /// `SET`s and the handshake gave them, over the cluster's global values.
    pub vars: &'a mut SessionVars,
    /// The statement's warnings (Go's `StmtCtx`).
    pub warnings: &'a mut Vec<SQLWarn>,
}

/// Read the string value of `sys_var` for the prototype, honoring test/server overrides
/// (`version`, `have_ssl`, `have_openssl`) while resolving every other variable from `vars`.
pub(crate) fn read_sysvar_string(
    vars: &mut SessionVars,
    sys_var: &SysVar,
    server_version: &str,
    have_ssl: &str,
    global: bool,
) -> Result<String, SqlError> {
    if sys_var.name == "version" && !server_version.is_empty() {
        return Ok(server_version.to_owned());
    }
    if matches!(sys_var.name, "have_ssl" | "have_openssl") && !have_ssl.is_empty() {
        return Ok(have_ssl.to_owned());
    }
    if sys_var.has_none_scope() {
        return Ok(sys_var.value.clone());
    }
    if global {
        vars.get_global_system_var(sys_var.name)
            .map_err(|e| sql_error(&e))
    } else {
        vars.get_session_or_global_system_var(sys_var.name)
            .map_err(|e| sql_error(&e))
    }
}

/// Check name/scope, emit noop read warning when disabled, and evaluate `@@[scope.]name` to a
/// typed `Datum` matching Go's `SysVar.GetNativeValType`.
pub(crate) fn eval_sysvar_datum(
    vars: &mut SessionVars,
    name: &str,
    global: bool,
    explicit_scope: bool,
    server_version: &str,
    have_ssl: &str,
    warnings: &mut Vec<SQLWarn>,
) -> Result<Datum, SqlError> {
    check_system_variable(name, global, explicit_scope)?;
    let sys_var = tidb_sessionctx::variable::get_sys_var(name)
        .expect("check_system_variable ensures registered");
    if sys_var.is_noop
        && !tidb_sessionctx::variable::tidb_vars::EnableNoopVariables
            .load(std::sync::atomic::Ordering::SeqCst)
    {
        warnings.push(SQLWarn {
            level: WarnLevelWarning.into(),
            err: ErrGettingNoopVariable.fast_gen_by_args(go_args![sys_var.name.to_owned()]),
        });
    }
    let val = read_sysvar_string(vars, &sys_var, server_version, have_ssl, global)?;
    let (native, _, _) = sys_var.get_native_val_type(&val);
    Ok(native)
}

impl ConstEnv<'_> {
    /// The value of `@@[global.]name` and its column, with Go's result types (Go
    /// `SysVar.GetNativeValType`: unsigned and boolean variables are LONGLONG, everything else
    /// a string in the connection charset).
    fn sysvar(&mut self, name: &str, global: bool) -> Result<(ColumnDef, Value), SqlError> {
        let sys_var = tidb_sessionctx::variable::get_sys_var(name)
            .expect("check_system_variable ensures registered");
        let val = read_sysvar_string(
            self.vars,
            &sys_var,
            self.server_version,
            self.have_ssl,
            global,
        )?;
        let (native, tp, flag) = sys_var.get_native_val_type(&val);
        Ok(match tp {
            tidb_base::mysql::TypeLonglong if flag & tidb_base::mysql::UnsignedFlag != 0 => (
                ColumnDef::new("", ColumnType::LongLong)
                    .with_flags(column_flag::UNSIGNED | column_flag::BINARY),
                Value::UInt(native.get_uint64()),
            ),
            tidb_base::mysql::TypeLonglong => (
                ColumnDef::new("", ColumnType::LongLong),
                Value::Int(native.get_int64()),
            ),
            _ => {
                let conn_cs = self.vars.get_system_var("character_set_connection").0;
                let mut col = ColumnDef::new("", ColumnType::VarString)
                    .with_length(5)
                    .with_decimals(31);
                let cs_id = u16::from(tidb_base::mysql::charset_name_to_id(&conn_cs));
                if cs_id != 0 {
                    col.charset = cs_id;
                }
                (col, Value::from(val))
            }
        })
    }

    fn func(&self, name: &str) -> Option<(ColumnDef, Value)> {
        // Go: VARCHAR(64) in utf8mb4 (length 256), decimals unspecified.
        let string = |v: Value| {
            let mut col = ColumnDef::new("", ColumnType::VarString)
                .with_length(256)
                .with_decimals(31);
            if v != Value::Null {
                col.flags |= column_flag::NOT_NULL;
            }
            Some((col, v))
        };
        match name {
            "version" => string(self.server_version.into()),
            "database" | "schema" => string(self.current_db.map_or(Value::Null, Value::from)),
            "user" | "session_user" | "system_user" => string(self.user.into()),
            "current_user" => string(self.current_user.into()),
            "connection_id" => {
                let col = ColumnDef::new("", ColumnType::LongLong)
                    .with_flags(column_flag::NOT_NULL | column_flag::UNSIGNED);
                Some((col, Value::UInt(self.connection_id)))
            }
            _ => None,
        }
    }

    fn item(&mut self, e: &Expr, params: &[Literal]) -> Result<(ColumnDef, Value), SqlError> {
        let unsupported = || SqlError::not_supported("this expression in a SELECT without FROM");
        match e {
            Expr::SysVar {
                name,
                global,
                explicit_scope,
            } => {
                check_system_variable(name, *global, *explicit_scope)?;
                if let Some(sv) = tidb_sessionctx::variable::get_sys_var(name)
                    && sv.is_noop
                    && !tidb_sessionctx::variable::tidb_vars::EnableNoopVariables
                        .load(std::sync::atomic::Ordering::SeqCst)
                {
                    // The variable does nothing, append a warning to the statement output.
                    self.warnings.push(SQLWarn {
                        level: WarnLevelWarning.into(),
                        err: ErrGettingNoopVariable.fast_gen_by_args(go_args![sv.name.to_owned()]),
                    });
                }
                self.sysvar(name, *global)
            }
            Expr::Func { name } => self.func(name).ok_or_else(unsupported),
            Expr::Literal(_) | Expr::Param(_) | Expr::Neg(_) => {
                Ok(literal_column(eval_const(e, params)?))
            }
            Expr::Column(c) => Err(SqlError::new(
                1054,
                format!("Unknown column '{}' in 'field list'", c.orig_col_name()),
            )),
            _ => Err(unsupported()),
        }
    }

    /// Evaluates a SELECT without FROM.
    pub fn select(&mut self, sel: &Select, params: &[Literal]) -> Result<ResultSet, SqlError> {
        if sel.where_.is_some() || !sel.group_by.is_empty() || !sel.order_by.is_empty() {
            return Err(SqlError::not_supported(
                "WHERE/GROUP BY/ORDER BY in a SELECT without FROM",
            ));
        }
        let mut columns = Vec::new();
        let mut row = Vec::new();
        for f in &sel.fields {
            let Field::Expr { expr, alias, text } = f else {
                return Err(SqlError::new(1096, "No tables used"));
            };
            let (mut col, v) = self.item(expr, params)?;
            col.name = if !alias.is_empty() {
                alias.clone()
            } else {
                match (expr, &v) {
                    // MySQL names a string literal column after its value.
                    (Expr::Literal(Literal::Str(s)), _) => s.clone(),
                    _ => text.clone(),
                }
            };
            col.org_name = String::new();
            columns.push(col);
            row.push(v);
        }
        let mut rows = vec![row];
        if let Some(l) = &sel.limit {
            let n = |e: &Expr| -> Result<u64, SqlError> {
                match eval_const(e, params)? {
                    Literal::Int(i) if i >= 0 => Ok(i as u64),
                    Literal::UInt(u) => Ok(u),
                    _ => Err(SqlError::new(1210, "Incorrect arguments to LIMIT")),
                }
            };
            let count = n(&l.count)?;
            let offset = l.offset.as_ref().map(n).transpose()?.unwrap_or(0);
            if count == 0 || offset > 0 {
                rows.clear();
            }
        }
        Ok(ResultSet {
            columns,
            rows,
            warnings: 0,
        })
    }
}

/// Column and value of a constant (Go: the literal's inferred type).
/// The checks Go makes before it reads a system variable in an expression
/// (`expressionRewriter.rewriteSystemVariable`, pkg/planner/core/expression_rewriter.go:1797),
/// in its order, over the ported sysvar registry (D05.1): an unknown variable is
/// `ErrUnknownSystemVar` (1193), or `ErrVariableNoLongerSupported` for a removed one; with an
/// explicit scope, `@@global.` of a session-only variable and `@@session.` of a global-only one
/// are `ErrIncorrectScope` (1238), except for a variable of no scope (read-only constants).
/// Errors go to the client as Go's `writeError` sends them.
///
/// Not done here: the warning Go appends when reading a no-op variable while
/// `tidb_enable_noop_variables` is off ([`ConstEnv`]'s caller appends it), and the
/// `RESTRICTED_VARIABLES_ADMIN` check under security enhanced mode (no SEM on this node).
// glue: the prototype's stand-in for the name and scope checks of expressionRewriter.rewriteSystemVariable (the function itself is the rewriter's part 2, D09.2, still a stub)
pub(crate) fn check_system_variable(
    name: &str,
    is_global: bool,
    explicit_scope: bool,
) -> Result<(), SqlError> {
    use tidb_sessionctx::variable::error::{ErrIncorrectScope, ErrUnknownSystemVar};
    let Some(sys_var) = tidb_sessionctx::variable::get_sys_var(name) else {
        let mut err = ErrUnknownSystemVar.fast_gen_by_args(go_args![name.to_owned()]);
        if let Err(removed) = tidb_sessionctx::variable::removed::check_sys_var_is_removed(name) {
            // Removed vars still return an error, but we customize it from
            // "unknown" to an explanation of why it is not supported.
            // This is important so users at least know they had the name correct.
            err = removed;
        }
        return Err(sql_error(&err));
    };
    if explicit_scope && !sys_var.has_none_scope() {
        if is_global && !(sys_var.has_global_scope() || sys_var.has_instance_scope()) {
            return Err(sql_error(
                &ErrIncorrectScope.gen_with_stack_by_args(go_args![name.to_owned(), "SESSION"]),
            ));
        }
        if !is_global && !sys_var.has_session_scope() {
            return Err(sql_error(
                &ErrIncorrectScope.gen_with_stack_by_args(go_args![name.to_owned(), "GLOBAL"]),
            ));
        }
    }
    Ok(())
}

fn literal_column(l: Literal) -> (ColumnDef, Value) {
    let not_null = column_flag::NOT_NULL;
    match l {
        Literal::Null => (ColumnDef::new("", ColumnType::Null), Value::Null),
        Literal::Bool(b) => (
            ColumnDef::new("", ColumnType::LongLong)
                .with_flags(not_null)
                .with_length(1),
            Value::Int(i64::from(b)),
        ),
        Literal::Int(i) => (
            ColumnDef::new("", ColumnType::LongLong)
                .with_flags(not_null)
                .with_length(i.to_string().len() as u32),
            Value::Int(i),
        ),
        Literal::UInt(u) => (
            ColumnDef::new("", ColumnType::LongLong)
                .with_flags(not_null | column_flag::UNSIGNED)
                .with_length(u.to_string().len() as u32),
            Value::UInt(u),
        ),
        Literal::Decimal(s) => {
            let frac = s.split_once('.').map_or(0, |(_, f)| f.len());
            // Go `DefaultTypeForValue`: flen = len + 1 (the point); the column length adds
            // the sign and, with a fraction, the point again.
            let len = s.len() + 2 + usize::from(frac > 0);
            (
                ColumnDef::new("", ColumnType::NewDecimal)
                    .with_flags(not_null)
                    .with_length(len as u32)
                    .with_decimals(frac as u8),
                Value::Decimal(s),
            )
        }
        Literal::Float(f) => (
            ColumnDef::new("", ColumnType::Double)
                .with_flags(not_null)
                .with_decimals(31),
            Value::Double(f),
        ),
        Literal::Str(s) => {
            let mut col = ColumnDef::new("", ColumnType::VarString)
                .with_flags(not_null)
                .with_length(s.chars().count() as u32 * 4)
                .with_decimals(31);
            col.charset = collation::UTF8MB4_BIN;
            (col, Value::from(s))
        }
    }
}

#[cfg(test)]
mod tests {
    use tidb_sql::Statement;

    use super::*;

    /// Go's compiled defaults with the session values a utf8mb4 handshake gives, and the
    /// statement's warnings.
    struct Fixture {
        session: tidb_session::Session,
        warnings: Vec<SQLWarn>,
    }

    fn fixture() -> Fixture {
        let mut session = crate::plan_cache::compiled_defaults_session().unwrap();
        session
            .vars_mut()
            .set_system_var_without_validation("collation_connection", "utf8mb4_general_ci")
            .unwrap();
        session.vars_mut().prev_found_in_plan_cache = true;
        Fixture {
            session,
            warnings: Vec::new(),
        }
    }

    impl Fixture {
        fn env(&mut self) -> ConstEnv<'_> {
            ConstEnv {
                server_version: "8.0.11-TiDB-tidb-rs",
                current_db: Some("sbtest"),
                user: "root@127.0.0.1",
                current_user: "root@%",
                connection_id: 9,
                have_ssl: "YES",
                vars: self.session.vars_mut(),
                warnings: &mut self.warnings,
            }
        }

        fn run(&mut self, sql: &str) -> Result<ResultSet, SqlError> {
            let l = tidb_sql::parse_and_lower(sql)?;
            let Statement::Select(s) = l.stmt else {
                panic!("{sql}")
            };
            self.env().select(&s, &[])
        }
    }

    /// VS1 plan §4.5: the previous statement's plan-cache state, as the router handed it.
    #[test]
    fn last_plan_from_cache_is_the_previous_statements() {
        let rs = run(
            "SELECT @@last_plan_from_cache, @@last_plan_from_binding",
            &[],
        )
        .unwrap();
        assert_eq!(rs.rows, vec![vec![Value::Int(1), Value::Int(0)]]);
    }

    fn run(sql: &str, params: &[Literal]) -> Result<ResultSet, SqlError> {
        let l = tidb_sql::parse_and_lower(sql)?;
        let Statement::Select(s) = l.stmt else {
            panic!("{sql}")
        };
        fixture().env().select(&s, params)
    }

    #[test]
    fn connect_time_queries() {
        let want_comment = tidb_sessionctx::variable::get_sys_var("version_comment")
            .unwrap()
            .value
            .clone();
        let r = run("select @@version_comment limit 1", &[]).unwrap();
        assert_eq!(r.rows, vec![vec![Value::from(want_comment)]]);
        assert_eq!(r.columns[0].name, "@@version_comment");
        let r = run("SELECT DATABASE(), USER() LIMIT 1", &[]).unwrap();
        assert_eq!(r.columns[1].name, "USER()");
        assert_eq!(
            r.rows[0],
            vec![Value::from("sbtest"), Value::from("root@127.0.0.1")]
        );
        let r = run("SELECT @@max_allowed_packet, @@autocommit", &[]).unwrap();
        assert_eq!(r.rows[0], vec![Value::UInt(64 << 20), Value::Int(1)]);
        assert!(r.columns[0].is_unsigned());
        assert_eq!(
            r.columns[0].flags,
            column_flag::UNSIGNED | column_flag::BINARY
        );
        let r = run("SELECT @@collation_connection", &[]).unwrap();
        assert_eq!(r.rows[0][0], Value::from("utf8mb4_general_ci"));
        // Go sets `have_ssl` = "YES" with TypeBool, and `GetNativeValType` calls
        // `tidb_opt_on("YES")` -> 0 (while `SHOW VARIABLES` returns "YES").
        let r = run("SELECT @@have_ssl", &[]).unwrap();
        assert_eq!(r.columns[0].column_type, ColumnType::LongLong);
        assert_eq!(r.rows[0][0], Value::Int(0));
        assert_eq!(run("SELECT @@no_such_var", &[]).unwrap_err().code, 1193);
    }

    /// The whole client error of a SELECT without FROM: code, SQLSTATE and message.
    fn err(sql: &str) -> (u16, &'static str, String) {
        let e = run(sql, &[]).unwrap_err();
        (e.code, e.state, e.message)
    }

    /// Go's name and scope checks of `rewriteSystemVariable` (#489): `@@global.` of a
    /// session-only variable and `@@session.` of a global-only one are 1238; a variable of no
    /// scope is read with any scope; an unknown one is 1193, a removed one 8136.
    #[test]
    fn system_variable_name_and_scope_checks() {
        let want_comment = tidb_sessionctx::variable::get_sys_var("version_comment")
            .unwrap()
            .value
            .clone();
        assert_eq!(
            err("SELECT @@global.warning_count"),
            (
                1238,
                "HY000",
                "Variable 'warning_count' is a SESSION variable".to_owned()
            )
        );
        assert_eq!(
            err("SELECT @@GLOBAL.Last_Plan_From_Cache"),
            (
                1238,
                "HY000",
                "Variable 'last_plan_from_cache' is a SESSION variable".to_owned()
            )
        );
        assert_eq!(
            err("SELECT @@session.tidb_gc_life_time"),
            (
                1238,
                "HY000",
                "Variable 'tidb_gc_life_time' is a GLOBAL variable".to_owned()
            )
        );
        assert_eq!(
            err("SELECT @@local.tidb_gc_life_time"),
            (
                1238,
                "HY000",
                "Variable 'tidb_gc_life_time' is a GLOBAL variable".to_owned()
            )
        );
        assert_eq!(
            err("SELECT @@no_such_var"),
            (
                1193,
                "HY000",
                "Unknown system variable 'no_such_var'".to_owned()
            )
        );
        assert_eq!(
            err("SELECT @@global.tidb_enable_streaming"),
            (
                8136,
                "HY000",
                "option 'tidb_enable_streaming' is no longer supported. Reason: streaming is \
                 no longer supported"
                    .to_owned()
            )
        );
        // No scope (a constant): the scope written is not checked.
        let r = run(
            "SELECT @@global.version_comment, @@session.version_comment",
            &[],
        )
        .unwrap();
        assert_eq!(
            r.rows[0],
            vec![Value::from(want_comment.clone()), Value::from(want_comment)]
        );
        // Session-only without a scope, global and session with either, and live variables.
        let r = run(
            "SELECT @@last_plan_from_cache, @@global.autocommit, @@session.autocommit, \
             @@warning_count, @@div_precision_increment",
            &[],
        )
        .unwrap();
        assert_eq!(
            r.rows[0],
            vec![
                Value::Int(1),
                Value::Int(1),
                Value::Int(1),
                Value::from("0"),
                Value::UInt(4),
            ]
        );
    }

    /// Every Go system variable is readable without error or panic.
    #[test]
    fn all_go_system_variables_are_readable() {
        let mut f = fixture();
        for sv in tidb_sessionctx::variable::get_sys_vars() {
            f.env().sysvar(sv.name, false).unwrap_or_else(|e| {
                panic!("session read failed for {}: {e:?}", sv.name);
            });
            if sv.has_global_scope() || sv.has_instance_scope() || sv.has_none_scope() {
                f.env().sysvar(sv.name, true).unwrap_or_else(|e| {
                    panic!("global read failed for {}: {e:?}", sv.name);
                });
            }
        }
    }

    #[test]
    fn literals_and_params() {
        let r = run("SELECT 1, -2, 'abc' AS s, 1.5, NULL, ?", &[Literal::Int(7)]).unwrap();
        assert_eq!(
            r.rows[0],
            vec![
                Value::Int(1),
                Value::Int(-2),
                Value::from("abc"),
                Value::Decimal("1.5".into()),
                Value::Null,
                Value::Int(7)
            ]
        );
        assert_eq!(r.columns[2].name, "s");
        assert_eq!(r.columns[3].column_length, 6);
        let r = run("SELECT 'Hello'", &[]).unwrap();
        assert_eq!(r.columns[0].name, "Hello");
        assert!(run("SELECT 1 LIMIT 0", &[]).unwrap().rows.is_empty());
        assert_eq!(run("SELECT c", &[]).unwrap_err().code, 1054);
        assert_eq!(run("SELECT *", &[]).unwrap_err().code, 1096);
        assert_eq!(run("SELECT 1 WHERE 1 = 1", &[]).unwrap_err().code, 1235);
    }

    /// What connectors read while connecting comes from the session variables, typed as Go
    /// types it (`GetNativeValType`): booleans and unsigned integers are LONGLONG, the rest
    /// strings.
    #[test]
    fn connector_reads_come_from_the_session_variables() {
        let mut f = fixture();
        let r = f
            .run(
                "SELECT @@init_connect, @@sql_select_limit, @@foreign_key_checks, \
                 @@group_concat_max_len, @@system_time_zone, @@global.sql_mode",
            )
            .unwrap();
        assert_eq!(
            r.rows[0],
            vec![
                Value::from(""),
                Value::UInt(u64::MAX),
                Value::Int(1),
                Value::UInt(1024),
                Value::from("CST"),
                Value::from(
                    "ONLY_FULL_GROUP_BY,STRICT_TRANS_TABLES,NO_ZERO_IN_DATE,NO_ZERO_DATE,\
                     ERROR_FOR_DIVISION_BY_ZERO,NO_AUTO_CREATE_USER,NO_ENGINE_SUBSTITUTION"
                ),
            ]
        );
        assert!(r.columns[1].is_unsigned());
        assert_eq!(r.columns[2].column_type, ColumnType::LongLong);
        assert_eq!(r.columns[4].column_type, ColumnType::VarString);
        // A session value written through the session variables is read back.
        f.session
            .vars_mut()
            .set_system_var("foreign_key_checks", "OFF")
            .unwrap();
        let r = f.run("SELECT @@foreign_key_checks").unwrap();
        assert_eq!(r.rows[0], vec![Value::Int(0)]);
        assert!(f.warnings.is_empty());
    }

    /// Go's `rewriteSystemVariable` warns when a noop variable is read
    /// (`ErrGettingNoopVariable`) only when `tidb_enable_noop_variables` is OFF; it is ON by
    /// default, so a connector's read of a noop variable has no warning.
    #[test]
    fn reading_a_noop_variable_with_the_defaults() {
        let mut f = fixture();
        let r = f.run("SELECT @@query_cache_type").unwrap();
        assert_eq!(r.rows[0], vec![Value::from("OFF")]);
        assert!(f.warnings.is_empty());
    }
}
