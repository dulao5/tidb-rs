//! Running statements on one server and capturing everything the comparison looks at.

use std::time::Duration;

use mysql_async::prelude::*;
use mysql_async::{Column, Conn, Opts, OptsBuilder, Params, SslOpts, Value};

/// Column definition fields compared between servers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnMeta {
    pub schema: String,
    pub table: String,
    pub org_table: String,
    pub name: String,
    pub org_name: String,
    pub column_type: u8,
    pub length: u32,
    pub decimals: u8,
    pub flags: u16,
    pub charset: u16,
}

impl ColumnMeta {
    pub fn from_column(c: &Column) -> Self {
        ColumnMeta {
            schema: c.schema_str().into_owned(),
            table: c.table_str().into_owned(),
            org_table: c.org_table_str().into_owned(),
            name: c.name_str().into_owned(),
            org_name: c.org_name_str().into_owned(),
            column_type: c.column_type() as u8,
            length: c.column_length(),
            decimals: c.decimals(),
            flags: c.flags().bits(),
            charset: c.character_set(),
        }
    }
}

/// Result of running a statement on one server.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Rows {
        columns: Vec<ColumnMeta>,
        /// Each value rendered by [`render`] (type-tagged, so `1` != `'1'`).
        rows: Vec<Vec<String>>,
        warnings: u16,
    },
    Ok {
        affected_rows: u64,
        warnings: u16,
    },
    Error {
        code: u16,
        state: String,
        message: String,
    },
    /// Connection, driver or timeout error (no server error code).
    Client(String),
}

/// What COM_STMT_PREPARE returned.
#[derive(Debug, Clone, PartialEq)]
pub struct PrepareMeta {
    pub param_count: usize,
    pub columns: Vec<ColumnMeta>,
}

/// Renders a value with its protocol type: strings are quoted, numbers keep their wire type
/// (text-protocol values are always strings; binary-protocol values are typed).
pub fn render(v: &Value) -> String {
    match v {
        Value::NULL => "NULL".into(),
        Value::Bytes(b) => match std::str::from_utf8(b) {
            Ok(s) => format!("{s:?}"),
            Err(_) => {
                let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
                format!("x'{hex}'")
            }
        },
        Value::Int(i) => i.to_string(),
        Value::UInt(u) => format!("{u}u"),
        Value::Float(f) => format!("{f:?}f32"),
        Value::Double(d) => format!("{d:?}f64"),
        Value::Date(y, mo, d, h, mi, s, us) => {
            format!("DATE({y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}.{us:06})")
        }
        Value::Time(neg, d, h, mi, s, us) => format!(
            "TIME({}{d}d {h:02}:{mi:02}:{s:02}.{us:06})",
            if *neg { "-" } else { "" }
        ),
    }
}

/// How to use TLS.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum TlsMode {
    /// TLS if the server offers it (self-signed certificates accepted), else plain.
    Prefer,
    /// Require TLS (self-signed certificates accepted).
    On,
    Off,
}

/// A connection to one server, reconnected on demand. Session statements that succeeded are
/// replayed after a reconnect so the session state matches the other side.
pub struct Side {
    pub label: &'static str,
    base: OptsBuilder,
    tls: TlsMode,
    timeout: Duration,
    conn: Option<Conn>,
    session_log: Vec<String>,
    /// Whether the current connection uses TLS (for the banner).
    pub using_tls: bool,
}

fn ssl() -> SslOpts {
    SslOpts::default()
        .with_danger_accept_invalid_certs(true)
        .with_danger_skip_domain_validation(true)
}

fn server_outcome(e: mysql_async::Error) -> Outcome {
    match e {
        mysql_async::Error::Server(se) => Outcome::Error {
            code: se.code,
            state: se.state,
            message: se.message,
        },
        other => Outcome::Client(other.to_string()),
    }
}

impl Side {
    pub fn new(
        label: &'static str,
        endpoint: &str,
        user: &str,
        password: &str,
        db: Option<&str>,
        tls: TlsMode,
        timeout: Duration,
    ) -> Result<Self, String> {
        let (host, port) = endpoint
            .rsplit_once(':')
            .ok_or_else(|| format!("endpoint {endpoint:?} is not host:port"))?;
        let port: u16 = port
            .parse()
            .map_err(|e| format!("bad port in {endpoint:?}: {e}"))?;
        let base = OptsBuilder::default()
            .ip_or_hostname(host)
            .tcp_port(port)
            .user(Some(user))
            .pass((!password.is_empty()).then_some(password))
            .db_name(db)
            .prefer_socket(false);
        Ok(Side {
            label,
            base,
            tls,
            timeout,
            conn: None,
            session_log: Vec::new(),
            using_tls: false,
        })
    }

    async fn connect(&mut self) -> Result<(), Outcome> {
        let attempt = |ssl_opts: Option<SslOpts>| {
            let opts = Opts::from(self.base.clone().ssl_opts(ssl_opts));
            let timeout = self.timeout;
            async move {
                match tokio::time::timeout(timeout, Conn::new(opts)).await {
                    Ok(r) => r.map_err(server_outcome),
                    Err(_) => Err(Outcome::Client("connect timed out".into())),
                }
            }
        };
        let (conn, tls) = match self.tls {
            TlsMode::Off => (attempt(None).await?, false),
            TlsMode::On => (attempt(Some(ssl())).await?, true),
            TlsMode::Prefer => match attempt(Some(ssl())).await {
                Ok(c) => (c, true),
                Err(Outcome::Error {
                    code,
                    state,
                    message,
                }) => {
                    return Err(Outcome::Error {
                        code,
                        state,
                        message,
                    });
                }
                Err(_) => (attempt(None).await?, false),
            },
        };
        self.conn = Some(conn);
        self.using_tls = tls;
        for stmt in self.session_log.clone() {
            // Best effort: it succeeded before on this server.
            let _ = self.run_text(&stmt).await;
        }
        Ok(())
    }

    /// Opens the connection now (to fail early with a clear message).
    pub async fn ensure_connected(&mut self) -> Result<(), String> {
        if self.conn.is_none() {
            self.connect().await.map_err(|o| match o {
                Outcome::Error { code, message, .. } => format!("error {code}: {message}"),
                Outcome::Client(m) => m,
                other => format!("{other:?}"),
            })?;
        }
        Ok(())
    }

    /// Remembers a session statement that succeeded, for replay after reconnects.
    pub fn log_session(&mut self, sql: &str) {
        self.session_log.push(sql.to_string());
    }

    /// Runs `sql` via COM_QUERY.
    pub async fn query(&mut self, sql: &str) -> Outcome {
        if let Err(o) = self.connect_if_needed().await {
            return o;
        }
        self.run_text(sql).await
    }

    async fn connect_if_needed(&mut self) -> Result<(), Outcome> {
        if self.conn.is_none() {
            self.connect().await?;
        }
        Ok(())
    }

    async fn run_text(&mut self, sql: &str) -> Outcome {
        let timeout = self.timeout;
        let Some(conn) = self.conn.as_mut() else {
            return Outcome::Client("not connected".into());
        };
        let fut = async {
            let mut res = conn.query_iter(sql).await?;
            collect(&mut res).await
        };
        let out = finish(timeout, fut).await;
        self.drop_conn_on_client_error(&out);
        out
    }

    /// Prepares `sql` (COM_STMT_PREPARE) and executes it with `params`. A prepare failure is
    /// returned as the outcome (with no metadata).
    pub async fn prepared(
        &mut self,
        sql: &str,
        params: &[Value],
    ) -> (Option<PrepareMeta>, Outcome) {
        if let Err(o) = self.connect_if_needed().await {
            return (None, o);
        }
        let timeout = self.timeout;
        let Some(conn) = self.conn.as_mut() else {
            return (None, Outcome::Client("not connected".into()));
        };
        let stmt = match tokio::time::timeout(timeout, conn.prep(sql)).await {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => {
                let out = server_outcome(e);
                self.drop_conn_on_client_error(&out);
                return (None, out);
            }
            Err(_) => {
                self.conn = None;
                return (
                    None,
                    Outcome::Client(format!("prepare timed out after {timeout:?}")),
                );
            }
        };
        let meta = PrepareMeta {
            param_count: stmt.params().len(),
            columns: stmt.columns().iter().map(ColumnMeta::from_column).collect(),
        };
        let params = if params.is_empty() {
            Params::Empty
        } else {
            Params::Positional(params.to_vec())
        };
        let fut = async {
            let mut res = conn.exec_iter(&stmt, params).await?;
            collect(&mut res).await
        };
        let out = finish(timeout, fut).await;
        self.drop_conn_on_client_error(&out);
        (Some(meta), out)
    }

    /// After a client-side error the connection state is unknown: reconnect next time.
    fn drop_conn_on_client_error(&mut self, out: &Outcome) {
        if matches!(out, Outcome::Client(_)) {
            self.conn = None;
        }
    }

    pub async fn disconnect(&mut self) {
        if let Some(c) = self.conn.take() {
            let _ = c.disconnect().await;
        }
    }
}

async fn finish(
    timeout: Duration,
    fut: impl Future<Output = Result<Outcome, mysql_async::Error>>,
) -> Outcome {
    match tokio::time::timeout(timeout, fut).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => server_outcome(e),
        Err(_) => Outcome::Client(format!("timed out after {timeout:?}")),
    }
}

/// Reads the first result set (or OK) and drains any further ones.
async fn collect<P: mysql_async::prelude::Protocol>(
    res: &mut mysql_async::QueryResult<'_, '_, P>,
) -> Result<Outcome, mysql_async::Error> {
    let columns: Vec<ColumnMeta> = res
        .columns_ref()
        .iter()
        .map(ColumnMeta::from_column)
        .collect();
    if columns.is_empty() {
        let out = Outcome::Ok {
            affected_rows: res.affected_rows(),
            warnings: res.warnings(),
        };
        while res.next().await?.is_some() {}
        return Ok(out);
    }
    let mut rows = Vec::new();
    while let Some(row) = res.next().await? {
        rows.push(row.unwrap().iter().map(render).collect());
    }
    let warnings = res.warnings();
    Ok(Outcome::Rows {
        columns,
        rows,
        warnings,
    })
}
