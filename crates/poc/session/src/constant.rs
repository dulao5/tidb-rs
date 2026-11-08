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
            Expr::SysVar { name, .. } => self
                .sysvar(name)
                .ok_or_else(|| SqlError::new(1193, format!("Unknown system variable '{name}'"))),
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
        }
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
