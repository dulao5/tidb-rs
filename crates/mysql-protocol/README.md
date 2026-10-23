# tidb-mysql-protocol

MySQL wire-protocol server layer for tidb-rs. It owns the wire: packet framing, handshake and
authentication, command dispatch, prepared statement state and result encoding. It aims to look
like TiDB's `pkg/server` from a client's point of view. SQL execution sits behind a small trait.

```
src/constants.rs  capability/status/command/type/flag constants (TiDB pkg/parser/mysql)
src/error.rs      SqlError (code, SQLSTATE, message), TiDB codes/states used here
src/codec.rs      length-encoded ints/strings, bounds-checked payload Reader
src/packet.rs     PacketIo: 4-byte headers, sequence ids, >= 16 MiB splitting, max_allowed_packet
src/auth.rs       salt, mysql_native_password scramble/verify, Credential
src/handshake.rs  HandshakeV10, HandshakeResponse41 parsing, AuthSwitchRequest
src/value.rs      ColumnDef, ColumnType, Value; column definitions, text and binary rows
src/params.rs     COM_STMT_EXECUTE parameter decoding (null bitmap, types, long data)
src/handler.rs    SessionFactory / SessionHandler: the execution boundary
src/conn.rs       per-connection state machine (handshake, auth switch, commands)
src/server.rs     tokio listener, one task per connection, connection ids, shutdown
```

## Execution boundary

```rust
pub trait SessionHandler: Send + 'static {
    fn query(&mut self, sql: &str) -> impl Future<Output = Result<Response, SqlError>> + Send;
    fn prepare(&mut self, stmt_id: u32, sql: &str) -> impl Future<Output = Result<PreparedInfo, SqlError>> + Send;
    fn execute(&mut self, stmt_id: u32, params: Vec<Param>) -> impl Future<Output = Result<Response, SqlError>> + Send;
    fn close(&mut self, stmt_id: u32);
    fn use_db(&mut self, db: &str) -> impl Future<Output = Result<(), SqlError>> + Send;
    fn status_flags(&self) -> u16 { status::AUTOCOMMIT }
}

pub trait SessionFactory: Send + Sync + 'static {
    type Session: SessionHandler;
    fn lookup_user(&self, user: &str, host: &str) -> impl Future<Output = Option<Credential>> + Send;
    fn open_session(&self, info: &ConnectionInfo) -> Result<Self::Session, SqlError>;
}
```

`Response` is either `Ok(OkInfo)` or `ResultSet { columns: Vec<ColumnDef>, rows: Vec<Vec<Value>> }`.
`Value` is protocol-centric (`Null`, `Int`, `UInt`, `Float`, `Double`, `Decimal(String)`,
`Bytes`, `DateTime`, `Time`). The executor converts its datums into these values. The column
type then picks the wire encoding, as in TiDB's `DumpBinaryRow` and `FormatValueText`.
Statement ids, parameter types and long data stay in the protocol layer. The handler only gets
decoded `Param`s.

## Behaviour mirrored from TiDB

- Capabilities: TiDB's `defaultCapability` minus what isn't implemented yet (compression,
  `LOCAL_FILES`, `MULTI_STATEMENTS`, and TLS, which TiDB only adds when configured). Server
  version `8.0.11-TiDB-<release>`, collation 46 (`utf8mb4_bin`), handshake status `AUTOCOMMIT`.
- Auth: only `mysql_native_password` is verified. As TiDB does for native-password accounts, the
  server sends an AuthSwitchRequest to `mysql_native_password` when the client answers with
  another plugin (e.g. libmysqlclient 8.x's `caching_sha2_password`), or when the handshake
  advertised another plugin (`--default-auth-plugin caching_sha2_password`). Access denied is
  1045/28000 with TiDB's message.
- `CLIENT_DEPRECATE_EOF` on and off: EOF after metadata only without it; the rows end with an OK
  packet (header 0xFE) with it, otherwise with an EOF packet.
- Errors: unknown command is 1105 `command N not supported now`, malformed packets are 1105
  `malform packet error`, unknown statement handles are 1243. Unsupported features use 1235
  (`ER_NOT_SUPPORTED_YET`, SQLSTATE 42000).
- Prepare: parameters are described as anonymous BLOB columns, like TiDB. Execute supports the
  new-params-bound flag (types are remembered for later executes), the null bitmap, all
  integer/float/double/decimal/string/blob/date/datetime/timestamp/time parameter types, and
  `COM_STMT_SEND_LONG_DATA`. The forbidden cursor flags are rejected like TiDB.
- Byte layouts are cross-checked in unit tests against TiDB's own test vectors: the handshake
  (`TestInitialHandshake`), a captured MySQL 8.0 handshake response, the native-password
  scramble (`TestCheckScramble`), binary date/time (`TestDumpBinaryTime`), and parameter
  decoding (`TestParseExecArgs`).

## Decision: own implementation rather than `opensrv-mysql`

The main existing crate is `opensrv-mysql` (Databend's fork of `msql-srv`). It was considered
and not used:

- Control over observable behaviour. In 0.7.0, the handshake hard-codes its capability set
  (no `CLIENT_TRANSACTIONS`, `LONG_FLAG`, `CONNECT_ATTRS`, ...) and sends status flags 0 instead
  of `AUTOCOMMIT`. Result encoding goes through its own value traits. Matching TiDB (capability
  flags, status flags, error and SQLSTATE mapping, column-type-driven binary encoding,
  auth-switch rules) would mean forking it anyway.
- Maintenance health. Its last release is 0.7.0 (2024-02); `msql-srv` was last released
  2023-12. A fork would be ours to maintain regardless.
- Future needs. Later we need `COM_STMT_FETCH` cursors, compression, TLS, session-state
  tracking, multi-statement results and streaming result sets. That is easier in about 2k lines
  we own and test byte-for-byte against TiDB than inside a third-party shim API.
- Cost. Without a third-party protocol crate, the runtime deps are only `tokio`, `sha1`,
  `getrandom` and `tracing`. `mysql_async` is a dev-dependency, used to test with a real client.

## Known gaps

- No TLS. TLS-mandatory clients cannot connect. One example is MariaDB Connector/C 3.4+, which
  is what Homebrew's `sysbench` links; libmysqlclient and the `mysql` CLI fall back to
  plaintext. No compression, no `caching_sha2_password` verification (always switched to
  native), no `COM_CHANGE_USER`.
- No multi-statements, so `CLIENT_MULTI_STATEMENTS` is not advertised. Cursors are not
  supported: `COM_STMT_FETCH` returns 1235, and `CURSOR_TYPE_READ_ONLY` gets the full result
  without `SERVER_STATUS_CURSOR_EXISTS`.
- Result sets are materialised (`Vec` of rows). Streaming can be added behind the same wire
  behaviour.
- Binary DATETIME parameters with a time-zone offset (13 bytes, MySQL 8.0.19+) are rejected as
  malformed. Charset conversion is not done; everything is treated as utf8mb4/binary.
- `max_allowed_packet` is fixed at 64 MiB. No `wait_timeout` idle disconnect yet.
