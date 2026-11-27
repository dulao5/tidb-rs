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
use tidb_base::go_args;

/// Comment returned for `@@version_comment` (shown by the `mysql` CLI banner).
pub const VERSION_COMMENT: &str =
    "tidb-rs read-only prototype (Apache License 2.0), MySQL 8.0 compatible";

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
    pub autocommit: bool,
    /// `@@character_set_results` etc. as set by `SET NAMES`.
    pub charset: &'a str,
    pub collation: &'a str,
    /// `@@last_plan_from_cache` / `@@last_plan_from_binding`: the previous statement's, as
    /// the router recorded it (VS1 plan §4.5; the PoC's own statements never use a cache).
    pub last_plan_from_cache: bool,
    pub last_plan_from_binding: bool,
}

impl ConstEnv<'_> {
    /// The few system variables clients read while connecting, with TiDB's defaults and
    /// result types (Go `SysVar.GetNativeValType`: unsigned and boolean variables are
    /// LONGLONG, everything else a string in the connection charset).
    fn sysvar(&self, name: &str) -> Option<(ColumnDef, Value)> {
        let unsigned = |v: u64| {
            let col = ColumnDef::new("", ColumnType::LongLong).with_flags(column_flag::UNSIGNED);
            Some((col, Value::UInt(v)))
        };
        let boolean = |v: bool| {
            Some((
                ColumnDef::new("", ColumnType::LongLong),
                Value::Int(i64::from(v)),
            ))
        };
        let string = |v: &str| {
            let col = ColumnDef::new("", ColumnType::VarString)
                .with_length(5)
                .with_decimals(31);
            Some((col, Value::from(v)))
        };
        match name {
            "version_comment" => string(VERSION_COMMENT),
            "version" => string(self.server_version),
            "max_allowed_packet" => unsigned(64 << 20),
            "wait_timeout" | "interactive_timeout" => unsigned(28800),
            "net_write_timeout" => unsigned(60),
            "net_read_timeout" => unsigned(30),
            "auto_increment_increment" | "auto_increment_offset" => unsigned(1),
            "autocommit" => boolean(self.autocommit),
            "last_plan_from_cache" => boolean(self.last_plan_from_cache),
            "last_plan_from_binding" => boolean(self.last_plan_from_binding),
            "transaction_read_only" | "tx_read_only" => boolean(false),
            "lower_case_table_names" => string("2"),
            "transaction_isolation" | "tx_isolation" => string("REPEATABLE-READ"),
            "character_set_client" | "character_set_connection" | "character_set_results" => {
                string(self.charset)
            }
            "character_set_server" | "character_set_database" => string("utf8mb4"),
            "collation_connection" => string(self.collation),
            "collation_server" | "collation_database" => string("utf8mb4_bin"),
            "time_zone" | "system_time_zone" => string("SYSTEM"),
            "have_ssl" | "have_openssl" => string(self.have_ssl),
            "license" => string("Apache License 2.0"),
            "sql_mode" => string(
                "ONLY_FULL_GROUP_BY,STRICT_TRANS_TABLES,NO_ZERO_IN_DATE,NO_ZERO_DATE,ERROR_FOR_DIVISION_BY_ZERO,NO_AUTO_CREATE_USER,NO_ENGINE_SUBSTITUTION",
            ),
            _ => None,
        }
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

    fn item(&self, e: &Expr, params: &[Literal]) -> Result<(ColumnDef, Value), SqlError> {
        let unsupported = || SqlError::not_supported("this expression in a SELECT without FROM");
        match e {
            Expr::SysVar {
                name,
                global,
                explicit_scope,
            } => {
                check_system_variable(name, *global, *explicit_scope)?;
                self.sysvar(name).ok_or_else(|| {
                    SqlError::not_supported(format!(
                        "reading system variable '{name}' (the prototype answers only the \
                         variables clients read while connecting)"
                    ))
                })
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
    pub fn select(&self, sel: &Select, params: &[Literal]) -> Result<ResultSet, SqlError> {
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
/// `tidb_enable_noop_variables` is off (the prototype keeps no warnings), and the
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

    fn env() -> ConstEnv<'static> {
        ConstEnv {
            server_version: "8.0.11-TiDB-tidb-rs",
            current_db: Some("sbtest"),
            user: "root@127.0.0.1",
            current_user: "root@%",
            connection_id: 9,
            have_ssl: "YES",
            autocommit: true,
            charset: "utf8mb4",
            collation: "utf8mb4_general_ci",
            last_plan_from_cache: true,
            last_plan_from_binding: false,
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
        env().select(&s, params)
    }

    #[test]
    fn connect_time_queries() {
        let r = run("select @@version_comment limit 1", &[]).unwrap();
        assert_eq!(r.rows, vec![vec![Value::from(VERSION_COMMENT)]]);
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
        let r = run("SELECT @@collation_connection", &[]).unwrap();
        assert_eq!(r.rows[0][0], Value::from("utf8mb4_general_ci"));
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
            vec![Value::from(VERSION_COMMENT), Value::from(VERSION_COMMENT)]
        );
        // Session-only without a scope, global and session with either.
        let r = run(
            "SELECT @@last_plan_from_cache, @@global.autocommit, @@session.autocommit",
            &[],
        )
        .unwrap();
        assert_eq!(r.rows[0], vec![Value::Int(1), Value::Int(1), Value::Int(1)]);
        // A known variable the prototype does not answer is not supported (it was 1193).
        assert_eq!(
            err("SELECT @@warning_count"),
            (
                1235,
                "42000",
                "This version of TiDB doesn't yet support 'reading system variable \
                 'warning_count' (the prototype answers only the variables clients read \
                 while connecting)'"
                    .to_owned()
            )
        );
    }

    /// Every variable the prototype answers is a Go system variable, so the checks above
    /// never reject one of them as unknown.
    #[test]
    fn answered_variables_are_go_system_variables() {
        for name in [
            "version_comment",
            "version",
            "max_allowed_packet",
            "wait_timeout",
            "interactive_timeout",
            "net_write_timeout",
            "net_read_timeout",
            "auto_increment_increment",
            "auto_increment_offset",
            "autocommit",
            "last_plan_from_cache",
            "last_plan_from_binding",
            "transaction_read_only",
            "tx_read_only",
            "lower_case_table_names",
            "transaction_isolation",
            "tx_isolation",
            "character_set_client",
            "character_set_connection",
            "character_set_results",
            "character_set_server",
            "character_set_database",
            "collation_connection",
            "collation_server",
            "collation_database",
            "time_zone",
            "system_time_zone",
            "have_ssl",
            "have_openssl",
            "license",
            "sql_mode",
        ] {
            assert!(env().sysvar(name).is_some(), "{name}");
            assert!(
                tidb_sessionctx::variable::get_sys_var(name).is_some(),
                "{name}"
            );
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
}
