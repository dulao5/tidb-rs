//! Compile-time and runtime version metadata (Go `pkg/util/versioninfo/versioninfo.go`).
//!
//! go-file: pkg/util/versioninfo/versioninfo.go
//!
//! Per owner decision D1 = A1 (ruling R35), the default release version uses SemVer build
//! metadata `v8.5.8+tidb-rs.<7-char-hash>` (at most one hyphen inside metadata) so Go TiDB's
//! `br/pkg/version` and `infoschema.FormatTiDBVersion` compare it as equal to `8.5.8` (never
//! `< 8.5.8`).

#![allow(non_upper_case_globals)]

use tidb_base::config::Config;
use tidb_base::mysql::format_server_version;

/// Default edition (Go `versioninfo.TiDBEdition`, Community).
pub const TIDB_EDITION: &str = "Community";
/// Compile-time UTC build timestamp (`YYYY-MM-DD HH:MM:SS`).
pub const TIDB_BUILD_TS: &str = env!("TIDB_RS_BUILD_TS");
/// Compile-time full git commit hash (or `"None"`).
pub const TIDB_GIT_HASH: &str = env!("TIDB_RS_GIT_HASH");
/// Compile-time git branch name (or `"None"`).
pub const TIDB_GIT_BRANCH: &str = env!("TIDB_RS_GIT_BRANCH");
/// Compile-time `rustc --version` first line (owner decision 2026-11-27).
pub const RUST_VERSION: &str = env!("TIDB_RS_RUST_VERSION");
/// Compile-time default release version (`v8.5.8+tidb-rs.<7-char-hash>`, ruling R35).
pub const DEFAULT_RELEASE_VERSION: &str = env!("TIDB_RS_RELEASE_VERSION");

/// Resolved TiDB release version (`cfg.tidb_release_version` if non-empty, else compile-time
/// [`DEFAULT_RELEASE_VERSION`]).
// glue: runtime release version resolution (cmd/tidb-server/main.go setGlobalVars)
pub fn release_version(cfg: &Config) -> String {
    if !cfg.tidb_release_version.is_empty() {
        cfg.tidb_release_version.clone()
    } else {
        DEFAULT_RELEASE_VERSION.to_owned()
    }
}

/// Resolved TiDB edition (`cfg.tidb_edition` if non-empty, else [`TIDB_EDITION`]).
// glue: runtime edition resolution (cmd/tidb-server/main.go setGlobalVars)
pub fn edition(cfg: &Config) -> String {
    if !cfg.tidb_edition.is_empty() {
        cfg.tidb_edition.clone()
    } else {
        TIDB_EDITION.to_owned()
    }
}

/// Full MySQL-format server version (`cfg.server_version` if explicitly configured, else
/// `8.0.11-TiDB-<release_version>`).
// glue: Go cmd/tidb-server/main.go:754-757 + mysql.ServerVersion formatting
pub fn server_version(cfg: &Config) -> String {
    if !cfg.server_version.is_empty() {
        return cfg.server_version.clone();
    }
    format_server_version(&release_version(cfg))
}

/// Default MySQL-format server version without config overrides (`8.0.11-TiDB-v8.5.8+tidb-rs.<7>`).
// glue: default server version helper for legacy/domain components
pub fn default_server_version() -> String {
    format_server_version(DEFAULT_RELEASE_VERSION)
}

/// Apply the resolved version to `tidb_base::mysql::{TiDBReleaseVersion, ServerVersion}`.
// glue: startup version initialization
pub fn init_version(cfg: &Config) {
    tidb_base::mysql::set_version(&release_version(cfg));
}
