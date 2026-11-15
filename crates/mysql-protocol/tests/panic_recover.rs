//! Go `clientConn.Run`'s deferred recover: a panic while a command runs is written to the
//! client as an error, counted in `tidb_server_panic_total{type="session"}`, and the
//! connection is closed (its session goes with it, so no half-run state survives).
//!
//! The panic is a real one of the ported code that Go has too: a point get whose schema
//! has `_tidb_row_checksum` reading a row in the old (v1) format, where both dereference
//! the missing column info of the checksum column (Go: a nil pointer).

use std::net::SocketAddr;
use std::sync::Arc;

use mysql_async::prelude::*;
use mysql_async::{Opts, OptsBuilder};
use tidb_mysql_protocol::{
    ConnectionInfo, Credential, Param, PreparedInfo, Response, Server, ServerConfig,
    SessionFactory, SessionHandler, SqlError,
};
use tokio::net::TcpListener;

/// `SELECT _tidb_row_checksum FROM t WHERE id = 1` on a v1 row: decodeOldRowValToChunk.
fn read_v1_row_with_checksum_column() {
    use tidb_base::gostd::time::utc;
    use tidb_base::mysql::{PriKeyFlag, TypeLonglong, TypeString};
    use tidb_chunk::chunk::new_chunk_with_capacity;
    use tidb_executor::builder::new_row_decoder;
    use tidb_executor::point_get::decode_row_val_to_chunk;
    use tidb_expression::Column;
    use tidb_expression::schema::new_schema;
    use tidb_kv::key::{Handle, IntHandle};
    use tidb_model::{ColumnInfo, ExtraRowChecksumID, StatePublic, TableInfo};
    use tidb_parser::model::CIStr;
    use tidb_sessionctx::exprctx::ExprContextRef;
    use tidb_sessionctx::exprstatic as es;
    use tidb_types::field_type::new_field_type;

    let mut id_ft = new_field_type(TypeLonglong);
    id_ft.set_flag(PriKeyFlag);
    let tbl = Arc::new(TableInfo {
        id: 1,
        name: CIStr::new("t"),
        columns: Some(vec![Arc::new(ColumnInfo {
            id: 1,
            name: CIStr::new("id"),
            offset: 0,
            field_type: id_ft,
            state: StatePublic,
            ..Default::default()
        })]),
        pk_is_handle: true,
        state: StatePublic,
        max_column_id: 1,
        ..Default::default()
    });
    let mut ft = new_field_type(TypeString);
    ft.set_charset("utf8mb4");
    ft.set_collate("utf8mb4_bin");
    let ft = Arc::new(ft);
    let schema = new_schema(vec![Arc::new(Column {
        ret_type: Arc::clone(&ft),
        id: ExtraRowChecksumID as i64,
        unique_id: ExtraRowChecksumID as i64,
        index: 0,
        ..Column::default()
    })]);
    let ev = es::new_eval_context(vec![]);
    let ctx: ExprContextRef = es::new_expr_context(vec![es::with_eval_ctx(ev)]);
    let rd = new_row_decoder(&ctx, utc(), &schema, &tbl);
    let mut chk = new_chunk_with_capacity(&[ft], 1);
    // An old-format row without columns (Go EncodeOldRow of none: the nil flag).
    let v1_row = [0u8];
    let _ = decode_row_val_to_chunk(
        &*ctx,
        &utc(),
        &schema,
        &tbl,
        &Handle::Int(IntHandle(1)),
        &v1_row,
        &mut chk,
        &rd,
    );
}

struct Factory;

impl SessionFactory for Factory {
    type Session = Session;

    async fn lookup_user(&self, _user: &str, _host: &str) -> Option<Credential> {
        Some(Credential::native_from_password(""))
    }

    fn open_session(&self, _info: &ConnectionInfo) -> Result<Session, SqlError> {
        Ok(Session)
    }
}

struct Session;

impl SessionHandler for Session {
    async fn query(&mut self, sql: &str) -> Result<Response, SqlError> {
        if sql.contains("_tidb_row_checksum") {
            read_v1_row_with_checksum_column();
        }
        Ok(Response::ok())
    }
    async fn prepare(&mut self, _stmt_id: u32, _sql: &str) -> Result<PreparedInfo, SqlError> {
        Ok(PreparedInfo::default())
    }
    async fn execute(&mut self, _stmt_id: u32, _params: Vec<Param>) -> Result<Response, SqlError> {
        Ok(Response::ok())
    }
    fn close(&mut self, _stmt_id: u32) {}
    async fn use_db(&mut self, _db: &str) -> Result<(), SqlError> {
        Ok(())
    }
}

async fn start() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        Server::new(ServerConfig::default(), Factory)
            .serve(listener, std::future::pending())
            .await
            .unwrap();
    });
    addr
}

fn opts(addr: SocketAddr) -> Opts {
    OptsBuilder::default()
        .ip_or_hostname(addr.ip().to_string())
        .tcp_port(addr.port())
        .user(Some("root"))
        .prefer_socket(false)
        .into()
}

#[test]
fn v1_row_with_checksum_column_panics() {
    let r = std::panic::catch_unwind(read_v1_row_with_checksum_column);
    assert!(
        r.is_err(),
        "Go panics here (nil column info); so does the port"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn panic_is_an_error_and_closes_the_connection() {
    let addr = start().await;
    let mut conn = mysql_async::Conn::new(opts(addr)).await.unwrap();
    conn.query_drop("SELECT 1").await.unwrap();
    let before = tidb_metrics::panic_total(tidb_metrics::PANIC_SESSION);
    let e = conn
        .query_drop("SELECT _tidb_row_checksum FROM t WHERE id = 1")
        .await
        .unwrap_err();
    match e {
        mysql_async::Error::Server(se) => {
            assert_eq!(se.code, 1105);
            assert!(
                se.message.contains("decodeOldRowValToChunk"),
                "{}",
                se.message
            );
        }
        e => panic!("want the server's error, got {e:?}"),
    }
    assert_eq!(
        tidb_metrics::panic_total(tidb_metrics::PANIC_SESSION),
        before + 1
    );
    // The connection was closed after the error.
    assert!(conn.query_drop("SELECT 1").await.is_err());
    // The server goes on: a new connection works.
    let mut conn = mysql_async::Conn::new(opts(addr)).await.unwrap();
    conn.query_drop("SELECT 1").await.unwrap();
}
