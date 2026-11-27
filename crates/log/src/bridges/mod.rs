//! Direct bridges from external logging facades (`log`, `tracing`, `slog-global`) into
//! [`crate::logger::UnifiedLoggerCore`].
//!
//! CRITICAL: NEVER call `slog_global::redirect_std_log(...)` — doing so conflicts with
//! `log::set_logger` and risks recursive log loops.

pub mod log;
pub mod slog;
pub mod tracing;

/// Install all three external facade bridges once at startup.
pub fn install_all() {
    log::install();
    tracing::install();
    slog::install();
}
