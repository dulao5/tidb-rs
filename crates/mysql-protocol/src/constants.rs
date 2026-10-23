//! Protocol constants, mirroring TiDB's `pkg/parser/mysql` package.

/// Capability flags (`CLIENT_*`).
pub mod capability {
    pub const CLIENT_LONG_PASSWORD: u32 = 1;
    pub const CLIENT_FOUND_ROWS: u32 = 1 << 1;
    pub const CLIENT_LONG_FLAG: u32 = 1 << 2;
    pub const CLIENT_CONNECT_WITH_DB: u32 = 1 << 3;
    pub const CLIENT_NO_SCHEMA: u32 = 1 << 4;
    pub const CLIENT_COMPRESS: u32 = 1 << 5;
    pub const CLIENT_ODBC: u32 = 1 << 6;
    pub const CLIENT_LOCAL_FILES: u32 = 1 << 7;
    pub const CLIENT_IGNORE_SPACE: u32 = 1 << 8;
    pub const CLIENT_PROTOCOL_41: u32 = 1 << 9;
    pub const CLIENT_INTERACTIVE: u32 = 1 << 10;
    pub const CLIENT_SSL: u32 = 1 << 11;
    pub const CLIENT_IGNORE_SIGPIPE: u32 = 1 << 12;
    pub const CLIENT_TRANSACTIONS: u32 = 1 << 13;
    pub const CLIENT_RESERVED: u32 = 1 << 14;
    pub const CLIENT_SECURE_CONNECTION: u32 = 1 << 15;
    pub const CLIENT_MULTI_STATEMENTS: u32 = 1 << 16;
    pub const CLIENT_MULTI_RESULTS: u32 = 1 << 17;
    pub const CLIENT_PS_MULTI_RESULTS: u32 = 1 << 18;
    pub const CLIENT_PLUGIN_AUTH: u32 = 1 << 19;
    pub const CLIENT_CONNECT_ATTRS: u32 = 1 << 20;
    pub const CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA: u32 = 1 << 21;
    pub const CLIENT_CAN_HANDLE_EXPIRED_PASSWORDS: u32 = 1 << 22;
    pub const CLIENT_SESSION_TRACK: u32 = 1 << 23;
    pub const CLIENT_DEPRECATE_EOF: u32 = 1 << 24;
    pub const CLIENT_OPTIONAL_RESULTSET_METADATA: u32 = 1 << 25;
    pub const CLIENT_ZSTD_COMPRESSION_ALGORITHM: u32 = 1 << 26;
    pub const CLIENT_QUERY_ATTRIBUTES: u32 = 1 << 27;

    /// TiDB's `defaultCapability` (`pkg/server/server.go`).
    pub const TIDB_DEFAULT: u32 = CLIENT_LONG_PASSWORD
        | CLIENT_LONG_FLAG
        | CLIENT_CONNECT_WITH_DB
        | CLIENT_PROTOCOL_41
        | CLIENT_TRANSACTIONS
        | CLIENT_SECURE_CONNECTION
        | CLIENT_FOUND_ROWS
        | CLIENT_MULTI_STATEMENTS
        | CLIENT_MULTI_RESULTS
        | CLIENT_LOCAL_FILES
        | CLIENT_CONNECT_ATTRS
        | CLIENT_PLUGIN_AUTH
        | CLIENT_INTERACTIVE
        | CLIENT_DEPRECATE_EOF
        | CLIENT_COMPRESS
        | CLIENT_ZSTD_COMPRESSION_ALGORITHM;

    /// What this server advertises: TiDB's default minus features not implemented yet
    /// (compression, LOAD DATA LOCAL, multi-statements).
    pub const SERVER_DEFAULT: u32 = TIDB_DEFAULT
        & !(CLIENT_COMPRESS
            | CLIENT_ZSTD_COMPRESSION_ALGORITHM
            | CLIENT_LOCAL_FILES
            | CLIENT_MULTI_STATEMENTS);
}

/// Server status flags (`SERVER_STATUS_*`).
pub mod status {
    pub const IN_TRANS: u16 = 0x0001;
    pub const AUTOCOMMIT: u16 = 0x0002;
    pub const MORE_RESULTS_EXISTS: u16 = 0x0008;
    pub const NO_GOOD_INDEX_USED: u16 = 0x0010;
    pub const NO_INDEX_USED: u16 = 0x0020;
    pub const CURSOR_EXISTS: u16 = 0x0040;
    pub const LAST_ROW_SEND: u16 = 0x0080;
    pub const DB_DROPPED: u16 = 0x0100;
    pub const NO_BACKSLASH_ESCAPES: u16 = 0x0200;
    pub const METADATA_CHANGED: u16 = 0x0400;
    pub const QUERY_WAS_SLOW: u16 = 0x0800;
    pub const PS_OUT_PARAMS: u16 = 0x1000;
    pub const IN_TRANS_READONLY: u16 = 0x2000;
    pub const SESSION_STATE_CHANGED: u16 = 0x4000;
}

/// Command bytes (`COM_*`).
pub mod command {
    pub const SLEEP: u8 = 0x00;
    pub const QUIT: u8 = 0x01;
    pub const INIT_DB: u8 = 0x02;
    pub const QUERY: u8 = 0x03;
    pub const FIELD_LIST: u8 = 0x04;
    pub const STATISTICS: u8 = 0x09;
    pub const PING: u8 = 0x0e;
    pub const CHANGE_USER: u8 = 0x11;
    pub const STMT_PREPARE: u8 = 0x16;
    pub const STMT_EXECUTE: u8 = 0x17;
    pub const STMT_SEND_LONG_DATA: u8 = 0x18;
    pub const STMT_CLOSE: u8 = 0x19;
    pub const STMT_RESET: u8 = 0x1a;
    pub const SET_OPTION: u8 = 0x1b;
    pub const STMT_FETCH: u8 = 0x1c;
    pub const RESET_CONNECTION: u8 = 0x1f;
}

/// Packet header bytes.
pub mod header {
    pub const OK: u8 = 0x00;
    pub const EOF: u8 = 0xfe;
    pub const ERR: u8 = 0xff;
    pub const AUTH_SWITCH_REQUEST: u8 = 0xfe;
    /// Marks a NULL in a text-protocol row (and a NULL length-encoded integer).
    pub const NULL_VALUE: u8 = 0xfb;
}

/// Column definition flags (`*_FLAG`).
pub mod column_flag {
    pub const NOT_NULL: u16 = 1;
    pub const PRI_KEY: u16 = 1 << 1;
    pub const UNIQUE_KEY: u16 = 1 << 2;
    pub const MULTIPLE_KEY: u16 = 1 << 3;
    pub const BLOB: u16 = 1 << 4;
    pub const UNSIGNED: u16 = 1 << 5;
    pub const ZEROFILL: u16 = 1 << 6;
    pub const BINARY: u16 = 1 << 7;
    pub const ENUM: u16 = 1 << 8;
    pub const AUTO_INCREMENT: u16 = 1 << 9;
    pub const TIMESTAMP: u16 = 1 << 10;
    pub const SET: u16 = 1 << 11;
    pub const NO_DEFAULT_VALUE: u16 = 1 << 12;
    pub const ON_UPDATE_NOW: u16 = 1 << 13;
    pub const NUM: u16 = 1 << 15;
}

/// Collation ids used on the wire.
pub mod collation {
    /// `utf8mb4_bin`, TiDB's `DefaultCollationID`.
    pub const UTF8MB4_BIN: u16 = 46;
    /// `binary`, used for numeric and temporal columns.
    pub const BINARY: u16 = 63;
}

/// `COM_STMT_EXECUTE` cursor flags.
pub mod cursor_type {
    pub const NO_CURSOR: u8 = 0;
    pub const READ_ONLY: u8 = 1;
    pub const FOR_UPDATE: u8 = 2;
    pub const SCROLLABLE: u8 = 4;
}

/// Authentication plugin names.
pub mod auth_plugin {
    pub const MYSQL_NATIVE_PASSWORD: &str = "mysql_native_password";
    pub const CACHING_SHA2_PASSWORD: &str = "caching_sha2_password";
}

/// Largest payload of a single wire packet; longer payloads are split.
pub const MAX_PAYLOAD_LEN: usize = 0xff_ffff;

/// Default `max_allowed_packet` (TiDB: 64 MiB).
pub const DEFAULT_MAX_ALLOWED_PACKET: usize = 64 << 20;

/// MySQL compatibility version TiDB reports (`mysqlCompatibilityVersion`).
pub const MYSQL_COMPATIBILITY_VERSION: &str = "8.0.11";

/// Server version string, in TiDB's `<mysql version>-TiDB-<release>` format so that tools which
/// parse TiDB's version keep working. The release part identifies this implementation.
pub const DEFAULT_SERVER_VERSION: &str = "8.0.11-TiDB-v8.4.0-tidb-rs";
