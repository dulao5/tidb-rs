//! Startup version banner, config dump, and `-V` / `SELECT tidb_version()` formatting.
//!
//! go-file: pkg/util/printer/printer.go

use std::sync::atomic::Ordering;

use tidb_base::config::{CheckTableBeforeDrop, Config};
use tidb_base::structured_log::{LogLevel, LogTarget};

use crate::logger;
use crate::versioninfo;

/// Print the startup `"Welcome to TiDB."` banner and `"loaded config"` JSON dump.
///
/// Temporarily sets the global log level to `INFO` and restores the previous level afterwards so
/// the banner is emitted even when started with `-L warn` or `-L error` (matching Go
/// `cmd/tidb-server/main.go:printInfo`).
///
/// Uses `tidb_base::config::marshal_json` (struct declaration order, exact integers) — NEVER
/// `get_json_config()`.
// go: PrintTiDBInfo L2 (GoVersion empty + Rust Version appended per owner decision 2026-11-27)
pub fn print_tidb_info(cfg: &Config) {
    let saved = logger::get_level();
    let raised = (saved as u8) > (LogLevel::Info as u8);
    if raised {
        let _ = logger::set_level_enum(LogLevel::Info);
    }

    let release_ver = versioninfo::release_version(cfg);
    let edition = versioninfo::edition(cfg);
    let check_before_drop = CheckTableBeforeDrop.load(Ordering::Relaxed);

    tidb_base::log_target!(
        LogTarget::Main,
        LogLevel::Info,
        "Welcome to TiDB.",
        "Release Version" => &release_ver,
        "Edition" => &edition,
        "Git Commit Hash" => versioninfo::TIDB_GIT_HASH,
        "Git Branch" => versioninfo::TIDB_GIT_BRANCH,
        "UTC Build Time" => versioninfo::TIDB_BUILD_TS,
        "GoVersion" => "",
        "Rust Version" => versioninfo::RUST_VERSION,
        "Race Enabled" => false,
        "Check Table Before Drop" => check_before_drop,
    );

    if let Ok(json_cfg) = tidb_base::config::marshal_json(cfg) {
        tidb_base::log_target!(
            LogTarget::Main,
            LogLevel::Info,
            "loaded config",
            "config" => &json_cfg,
        );
    }

    if raised {
        let _ = logger::set_level_enum(saved);
    }
}

/// Multi-line TiDB version summary for `-V` / `--version` and `SELECT tidb_version()`.
// go: GetTiDBInfo L2 (GoVersion empty + Rust Version appended per owner decision 2026-11-27 / R35)
pub fn get_tidb_info(cfg: &Config) -> String {
    let release_ver = versioninfo::release_version(cfg);
    let edition = versioninfo::edition(cfg);
    let check_before_drop = CheckTableBeforeDrop.load(Ordering::Relaxed);
    format!(
        "Release Version: {release_ver}\n\
         Edition: {edition}\n\
         Git Commit Hash: {}\n\
         Git Branch: {}\n\
         UTC Build Time: {}\n\
         GoVersion: \n\
         Rust Version: {}\n\
         Race Enabled: false\n\
         Check Table Before Drop: {check_before_drop}\n\
         Store: {}",
        versioninfo::TIDB_GIT_HASH,
        versioninfo::TIDB_GIT_BRANCH,
        versioninfo::TIDB_BUILD_TS,
        versioninfo::RUST_VERSION,
        cfg.store,
    )
}
