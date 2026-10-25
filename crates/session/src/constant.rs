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
    /// The few system variables clients read while connecting, with TiDB's defaults.
    fn sysvar(&self, name: &str) -> Option<(ColumnType, Value)> {
        let int = |v: i64| Some((ColumnType::LongLong, Value::Int(v)));
        let string = |v: &str| Some((ColumnType::VarString, Value::from(v)));
        match name {
            "version_comment" => string(VERSION_COMMENT),
            "version" => string(self.server_version),
            "max_allowed_packet" => int(64 << 20),
            "wait_timeout" | "interactive_timeout" => int(28800),
            "net_write_timeout" => int(60),
            "net_read_timeout" => int(30),
            "autocommit" => int(i64::from(self.autocommit)),
            "auto_increment_increment" | "auto_increment_offset" => int(1),
            "lower_case_table_names" => int(2),
            "transaction_isolation" | "tx_isolation" => string("REPEATABLE-READ"),
            "transaction_read_only" | "tx_read_only" => int(0),
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
        let string = |v: Value| Some((ColumnDef::new("", ColumnType::VarString), v));
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
            Expr::SysVar { name, .. } => {
                let (ty, v) = self.sysvar(name).ok_or_else(|| {
                    SqlError::new(1193, format!("Unknown system variable '{name}'"))
                })?;
                Ok((ColumnDef::new("", ty), v))
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
            (
                ColumnDef::new("", ColumnType::NewDecimal)
                    .with_flags(not_null)
                    .with_length(s.len() as u32)
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
                .with_length(s.chars().count() as u32 * 4);
            col.charset = collation::UTF8MB4_BIN;
            (col, Value::from(s))
        }
    }
}
