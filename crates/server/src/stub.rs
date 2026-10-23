//! Throwaway session handler used until the SQL layer exists. It recognises a handful of
//! statements by string matching, which is enough for the `mysql` CLI, connectors' connection
//! setup, and end-to-end tests of the text and binary protocols. It does not parse SQL.

use tidb_mysql_protocol::constants::column_flag;
use tidb_mysql_protocol::{
    ColumnDef, ColumnType, ConnectionInfo, Credential, Param, PreparedInfo, Response, ResultSet,
    SessionFactory, SessionHandler, SqlError, Value,
};

/// Comment returned for `@@version_comment` (shown by the `mysql` CLI banner).
pub const VERSION_COMMENT: &str =
    "tidb-rs read-only prototype (Apache License 2.0), MySQL 8.0 compatible";

/// Creates [`StubSession`]s and authenticates a single configured account.
#[derive(Debug)]
pub struct StubFactory {
    pub user: String,
    pub credential: Credential,
    pub server_version: String,
    /// Whether TLS is enabled (`@@have_ssl`).
    pub tls_enabled: bool,
}

impl SessionFactory for StubFactory {
    type Session = StubSession;

    async fn lookup_user(&self, user: &str, _host: &str) -> Option<Credential> {
        (user == self.user).then(|| self.credential.clone())
    }

    fn open_session(&self, info: &ConnectionInfo) -> Result<StubSession, SqlError> {
        let host = info
            .peer
            .rsplit_once(':')
            .map_or(info.peer.as_str(), |(h, _)| h);
        Ok(StubSession {
            server_version: self.server_version.clone(),
            current_db: None,
            user: format!(
                "{}@{}",
                info.user,
                host.trim_matches(|c| c == '[' || c == ']')
            ),
            current_user: format!("{}@%", info.user),
            connection_id: info.connection_id,
            have_ssl: if self.tls_enabled { "YES" } else { "DISABLED" },
            stmts: Default::default(),
        })
    }
}

/// See the module docs.
#[derive(Debug)]
pub struct StubSession {
    server_version: String,
    current_db: Option<String>,
    /// `USER()`: `user@client-host`.
    user: String,
    /// `CURRENT_USER()`: the matched account, `user@%`.
    current_user: String,
    connection_id: u64,
    have_ssl: &'static str,
    /// Prepared statements: id -> normalised SQL.
    stmts: std::collections::HashMap<u32, String>,
}

/// Lower-cases, collapses whitespace and drops a trailing `;`.
fn normalize(sql: &str) -> String {
    let sql = sql.trim().trim_end_matches(';').trim();
    sql.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

fn unsupported(sql: &str) -> SqlError {
    let mut s: String = sql.trim().chars().take(64).collect();
    if sql.trim().chars().count() > 64 {
        s.push_str("...");
    }
    SqlError::not_supported(s)
}

impl StubSession {
    /// The few system variables clients read while connecting, with TiDB's defaults.
    fn sysvar(&self, name: &str) -> Option<(ColumnDef, Value)> {
        let int = |v: i64| Some((ColumnDef::new("", ColumnType::LongLong), Value::Int(v)));
        let string = |v: &str| Some((ColumnDef::new("", ColumnType::VarString), Value::from(v)));
        match name {
            "version_comment" => string(VERSION_COMMENT),
            "version" => string(&self.server_version),
            "max_allowed_packet" => int(64 << 20),
            "wait_timeout" | "interactive_timeout" => int(28800),
            "autocommit" => int(1),
            "auto_increment_increment" | "auto_increment_offset" => int(1),
            "lower_case_table_names" => int(2),
            "transaction_isolation" | "tx_isolation" => string("REPEATABLE-READ"),
            "transaction_read_only" | "tx_read_only" => int(0),
            "character_set_client"
            | "character_set_connection"
            | "character_set_results"
            | "character_set_server"
            | "character_set_database" => string("utf8mb4"),
            "collation_connection" | "collation_server" | "collation_database" => {
                string("utf8mb4_bin")
            }
            "time_zone" | "system_time_zone" => string("SYSTEM"),
            "have_ssl" | "have_openssl" => string(self.have_ssl),
            "sql_mode" => string(
                "ONLY_FULL_GROUP_BY,STRICT_TRANS_TABLES,NO_ZERO_IN_DATE,NO_ZERO_DATE,ERROR_FOR_DIVISION_BY_ZERO,NO_AUTO_CREATE_USER,NO_ENGINE_SUBSTITUTION",
            ),
            _ => None,
        }
    }

    /// Evaluates one select-list item the stub knows (`item` with its original spelling).
    fn item(&self, item: &str) -> Option<(ColumnDef, Value)> {
        let lower = item.to_ascii_lowercase();
        let string = |v: Value| Some((ColumnDef::new("", ColumnType::VarString), v));
        match lower.as_str() {
            "version()" => return string(self.server_version.as_str().into()),
            "database()" | "schema()" => {
                return string(self.current_db.as_deref().map_or(Value::Null, Value::from));
            }
            "user()" | "session_user()" | "system_user()" => {
                return string(self.user.as_str().into());
            }
            "current_user()" | "current_user" => return string(self.current_user.as_str().into()),
            "connection_id()" => {
                let col = ColumnDef::new("", ColumnType::LongLong)
                    .with_flags(column_flag::NOT_NULL | column_flag::UNSIGNED);
                return Some((col, Value::UInt(self.connection_id)));
            }
            _ => {}
        }
        if let Some(var) = lower.strip_prefix("@@") {
            let var = var
                .strip_prefix("session.")
                .or_else(|| var.strip_prefix("global."))
                .unwrap_or(var);
            return self.sysvar(var);
        }
        // Integer literal: SELECT 1, SELECT -42.
        if let Ok(i) = item.parse::<i64>() {
            let col = ColumnDef::new("", ColumnType::LongLong)
                .with_flags(column_flag::NOT_NULL)
                .with_length(item.len() as u32);
            return Some((col, Value::Int(i)));
        }
        // Single-quoted string literal without embedded quotes: SELECT 'abc'.
        let s = item.strip_prefix('\'')?.strip_suffix('\'')?;
        if s.contains('\'') {
            return None;
        }
        let col = ColumnDef::new("", ColumnType::VarString).with_flags(column_flag::NOT_NULL);
        Some((col, s.into()))
    }

    /// Answers `SELECT <item>, <item>... [LIMIT 1]` when every item is one the stub knows
    /// (see [`Self::item`]); column names keep the original spelling.
    fn constant_select(&self, original: &str) -> Option<Response> {
        let sql = original.trim().trim_end_matches(';').trim();
        if sql.len() < 7 || !sql[..7].eq_ignore_ascii_case("select ") {
            return None;
        }
        let mut list = sql[7..].trim();
        if let Some(n) = list.len().checked_sub(" limit 1".len())
            && list.is_char_boundary(n)
            && list[n..].eq_ignore_ascii_case(" limit 1")
        {
            list = list[..n].trim();
        }
        let mut columns = Vec::new();
        let mut row = Vec::new();
        // Splitting on ',' is good enough for the items above (no commas inside them).
        for item in list.split(',') {
            let item = item.trim();
            let (mut col, v) = self.item(item)?;
            col.name = match &v {
                // MySQL names a string literal column after its value.
                Value::Bytes(b) if item.starts_with('\'') => {
                    String::from_utf8_lossy(b).into_owned()
                }
                _ => item.to_string(),
            };
            col.org_name = String::new();
            columns.push(col);
            row.push(v);
        }
        Some(Response::ResultSet(ResultSet {
            columns,
            rows: vec![row],
            warnings: 0,
        }))
    }
}

/// Column describing an echoed parameter, typed after the value the client sent.
fn param_column(p: &Param) -> ColumnDef {
    let ty = match &p.value {
        Value::Null => ColumnType::Null,
        Value::Int(_) | Value::UInt(_) => ColumnType::LongLong,
        Value::Float(_) => ColumnType::Float,
        Value::Double(_) => ColumnType::Double,
        Value::Decimal(_) => ColumnType::NewDecimal,
        Value::Bytes(_) => match p.ty {
            ColumnType::Blob
            | ColumnType::TinyBlob
            | ColumnType::MediumBlob
            | ColumnType::LongBlob => ColumnType::Blob,
            _ => ColumnType::VarString,
        },
        Value::DateTime(_) => match p.ty {
            ColumnType::Date => ColumnType::Date,
            _ => ColumnType::Datetime,
        },
        Value::Time(_) => ColumnType::Duration,
    };
    let mut col = ColumnDef::new("?", ty);
    col.org_name = String::new();
    if matches!(p.value, Value::UInt(_)) {
        col.flags |= column_flag::UNSIGNED;
    }
    match &p.value {
        Value::DateTime(t) if t.microsecond != 0 => col.decimals = 6,
        Value::Time(d) if d.microseconds != 0 => col.decimals = 6,
        Value::Float(_) | Value::Double(_) => col.decimals = 31,
        _ => {}
    }
    if ty == ColumnType::Blob {
        col.charset = tidb_mysql_protocol::constants::collation::BINARY;
        col.flags |= column_flag::BINARY | column_flag::BLOB;
    }
    col
}

impl SessionHandler for StubSession {
    async fn query(&mut self, sql: &str) -> Result<Response, SqlError> {
        let norm = normalize(sql);
        if let Some(resp) = self.constant_select(sql) {
            return Ok(resp);
        }
        if norm.starts_with("set ") {
            // SET NAMES ..., SET autocommit = 1, SET SESSION ...: accepted and ignored.
            if norm.contains("autocommit")
                && (norm.ends_with("= 0") || norm.ends_with("=0") || norm.ends_with("off"))
            {
                return Err(SqlError::not_supported(
                    "autocommit=0 (read-only autocommit node)",
                ));
            }
            return Ok(Response::ok());
        }
        if norm.starts_with("use ") {
            // `norm` starts with "use ", so the original text starts with "use" + whitespace.
            let db = sql.trim().trim_end_matches(';').trim()[3..]
                .trim()
                .trim_matches('`');
            self.use_db(db).await?;
            return Ok(Response::ok());
        }
        Err(unsupported(sql))
    }

    async fn prepare(&mut self, stmt_id: u32, sql: &str) -> Result<PreparedInfo, SqlError> {
        let norm = normalize(sql);
        if norm == "select ?" {
            self.stmts.insert(stmt_id, norm);
            return Ok(PreparedInfo {
                param_count: 1,
                columns: vec![ColumnDef::new("?", ColumnType::VarString)],
            });
        }
        if let Some(Response::ResultSet(rs)) = self.constant_select(sql) {
            self.stmts.insert(stmt_id, sql.to_string());
            return Ok(PreparedInfo {
                param_count: 0,
                columns: rs.columns,
            });
        }
        Err(unsupported(sql))
    }

    async fn execute(&mut self, stmt_id: u32, params: Vec<Param>) -> Result<Response, SqlError> {
        let sql = self
            .stmts
            .get(&stmt_id)
            .ok_or_else(|| SqlError::unknown_stmt(stmt_id, "stmt_execute"))?;
        if sql == "select ?" {
            let p = params.into_iter().next().unwrap_or_else(Param::null);
            let col = param_column(&p);
            return Ok(Response::ResultSet(ResultSet {
                columns: vec![col],
                rows: vec![vec![p.value]],
                warnings: 0,
            }));
        }
        self.constant_select(sql).ok_or_else(|| unsupported(sql))
    }

    fn close(&mut self, stmt_id: u32) {
        self.stmts.remove(&stmt_id);
    }

    async fn use_db(&mut self, db: &str) -> Result<(), SqlError> {
        // No catalog yet: any database name is accepted.
        self.current_db = Some(db.to_string());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> StubSession {
        StubSession {
            server_version: "v".into(),
            current_db: None,
            user: "root@127.0.0.1".into(),
            current_user: "root@%".into(),
            connection_id: 9,
            have_ssl: "YES",
            stmts: Default::default(),
        }
    }

    fn rs(r: Response) -> ResultSet {
        match r {
            Response::ResultSet(rs) => rs,
            other => panic!("expected result set, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn queries() {
        let mut s = session();
        let r = rs(s.query("SELECT 1").await.unwrap());
        assert_eq!(r.columns[0].name, "1");
        assert_eq!(r.rows, vec![vec![Value::Int(1)]]);
        let r = rs(s.query("select @@version_comment limit 1").await.unwrap());
        assert_eq!(r.rows[0][0], Value::from(VERSION_COMMENT));
        let r = rs(s
            .query("SELECT @@max_allowed_packet,@@SESSION.wait_timeout")
            .await
            .unwrap());
        assert_eq!(r.columns[0].name, "@@max_allowed_packet");
        assert_eq!(r.columns[1].name, "@@SESSION.wait_timeout");
        assert_eq!(r.rows[0], vec![Value::Int(64 << 20), Value::Int(28800)]);
        assert!(s.query("select @@no_such_var").await.is_err());
        let r = rs(s.query("SELECT  'Hello' ;").await.unwrap());
        assert_eq!(r.columns[0].name, "Hello");
        assert_eq!(r.rows[0][0], Value::from("Hello"));
        assert_eq!(s.query("SET NAMES utf8mb4").await.unwrap(), Response::ok());
        assert_eq!(s.query("set autocommit=1").await.unwrap(), Response::ok());
        assert!(s.query("set autocommit=0").await.is_err());
        assert_eq!(s.query("USE `sbtest`").await.unwrap(), Response::ok());
        let r = rs(s.query("select database()").await.unwrap());
        assert_eq!(r.rows[0][0], Value::from("sbtest"));
        // What the mysql CLI's `status` command sends.
        let r = rs(s.query("select DATABASE(), USER() limit 1").await.unwrap());
        assert_eq!(r.columns[1].name, "USER()");
        assert_eq!(
            r.rows[0],
            vec![Value::from("sbtest"), Value::from("root@127.0.0.1")]
        );
        let r = rs(s.query("SELECT CONNECTION_ID(), @@have_ssl").await.unwrap());
        assert_eq!(r.rows[0], vec![Value::UInt(9), Value::from("YES")]);
        assert!(s.query("select database(), nope()").await.is_err());
        let e = s.query("select * from t").await.unwrap_err();
        assert_eq!(e.code, 1235);
        assert_eq!(
            e.message,
            "This version of TiDB doesn't yet support 'select * from t'"
        );
    }

    #[tokio::test]
    async fn prepared_echo() {
        let mut s = session();
        let info = s.prepare(7, "SELECT ?").await.unwrap();
        assert_eq!(info.param_count, 1);
        let p = Param {
            ty: ColumnType::LongLong,
            unsigned: false,
            value: Value::Int(5),
        };
        let r = rs(s.execute(7, vec![p]).await.unwrap());
        assert_eq!(r.columns[0].column_type, ColumnType::LongLong);
        assert_eq!(r.rows[0][0], Value::Int(5));
        s.close(7);
        assert!(s.execute(7, vec![]).await.is_err());
        assert!(s.prepare(8, "select a from t where id = ?").await.is_err());
    }
}
