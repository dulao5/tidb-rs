//! Runtime logging, text/JSON encoders, file rotation, panic hook, and version banner for
//! tidb-rs (`D00.3`: `pkg/util/{logutil,printer,versioninfo}`).
//!
//! Placed at rank 1 in `layers.toml` (depends only on rank 0 `tidb-base` and workspace-locked
//! crates). Registers its [`logger::UnifiedLoggerCore`] into `tidb_base::structured_log` at
//! startup so all layered crates (ranks 0..=16) emit Go-v8.5.8-compatible single-line logs.

#![forbid(unsafe_code)]

pub mod bridges;
pub mod encoder;
pub mod logger;
pub mod panic_hook;
pub mod printer;
pub mod sink;
pub mod versioninfo;

pub use encoder::{EncoderConfig, Entry, LogFormat, OwnedLogField, OwnedLogValue};
pub use logger::{
    ContextLogger, get_level, init_logger, init_logger_with_sinks, is_initialized, replace_logger,
    set_level, set_level_enum, slow_query_sink, sync,
};
pub use panic_hook::set_panic_hook;
pub use printer::{get_tidb_info, print_tidb_info};
pub use sink::{
    BufferedSink, LockWithTimeoutSink, LogSink, MemorySink, RotatingFileSink, StdoutSink,
};
pub use versioninfo::{
    DEFAULT_RELEASE_VERSION, RUST_VERSION, TIDB_BUILD_TS, TIDB_EDITION, TIDB_GIT_BRANCH,
    TIDB_GIT_HASH, default_server_version, edition, init_version, release_version, server_version,
    version_comment,
};
