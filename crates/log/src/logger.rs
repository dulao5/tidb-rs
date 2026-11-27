//! Unified runtime logger core, atomic hot-swap (`replace_logger`), `ErrVerboseLogger`,
//! `GeneralLogger`, reserved `SlowQueryLogger` sink, and `ContextLogger`.
//!
//! go-file: pkg/util/logutil/log.go

use std::io;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Once};

use arc_swap::ArcSwap;
use tidb_base::Error;
use tidb_base::config::{LogConfig, ZaplogFileLogConfig};
use tidb_base::errors::format_backtrace_string;
use tidb_base::structured_log::{LogField, LogLevel, LogRecord, LogTarget, StructuredLogger};

use crate::bridges;
use crate::encoder::{self, EncoderConfig, Entry, LogFormat, OwnedLogField};
use crate::panic_hook;
use crate::sink::{self, LogSink, StdoutSink};

/// Immutable snapshot of active logger settings and sinks swapped atomically by `replace_logger`.
pub struct LoggerState {
    pub format: LogFormat,
    pub encoder_cfg: EncoderConfig,
    pub bound_fields: Vec<OwnedLogField>,
    pub keyspace_name: String,
    pub config: LogConfig,
    pub main_sink: Arc<dyn LogSink>,
    pub general_sink: Arc<dyn LogSink>,
    /// Reserved sink for `D14.2` slow query logger (`log.slow-query-file`).
    pub slow_query_sink: Option<Arc<dyn LogSink>>,
}

impl LoggerState {
    fn default_stdout() -> Self {
        let main_sink: Arc<dyn LogSink> = Arc::new(StdoutSink);
        Self {
            format: LogFormat::Text,
            encoder_cfg: EncoderConfig::default(),
            bound_fields: Vec::new(),
            keyspace_name: String::new(),
            config: LogConfig::default(),
            general_sink: Arc::clone(&main_sink),
            main_sink,
            slow_query_sink: None,
        }
    }

    fn from_config(
        cfg: &LogConfig,
        keyspace_name: &str,
        override_main: Option<Arc<dyn LogSink>>,
        override_general: Option<Arc<dyn LogSink>>,
    ) -> Result<(Self, LogLevel), Error> {
        let format = LogFormat::parse(&cfg.format)?;
        let level = LogLevel::parse_level(&cfg.level)?;
        let main_sink = match override_main {
            Some(s) => s,
            None => sink::build_sink(&cfg.file, cfg.timeout)?,
        };
        let general_sink = match override_general {
            Some(s) => s,
            None if cfg.general_log_file.is_empty() => Arc::clone(&main_sink),
            None => {
                let mut gcfg: ZaplogFileLogConfig = cfg.file.clone();
                gcfg.filename = cfg.general_log_file.clone();
                sink::build_sink(&gcfg, cfg.timeout)?
            }
        };
        let slow_query_sink = if cfg.slow_query_file.is_empty() {
            None
        } else if cfg.slow_query_file == cfg.file.filename {
            Some(Arc::clone(&main_sink))
        } else {
            // Reserved for D14.2: only eagerly open when an explicit file path is requested
            // alongside main file, otherwise opened lazily by D14.2.
            None
        };
        let mut bound_fields = Vec::new();
        if !keyspace_name.is_empty() {
            bound_fields.push(OwnedLogField::string("keyspaceName", keyspace_name));
        }
        let state = Self {
            format,
            encoder_cfg: EncoderConfig {
                disable_timestamp: cfg.disable_timestamp,
                disable_caller: false,
                disable_error_verbose: cfg.disable_error_verbose,
                force_error_verbose: false,
            },
            bound_fields,
            keyspace_name: keyspace_name.to_owned(),
            config: cfg.clone(),
            main_sink,
            general_sink,
            slow_query_sink,
        };
        Ok((state, level))
    }
}

/// Process-wide unified logger core registered into `tidb_base::structured_log`.
pub struct UnifiedLoggerCore {
    state: ArcSwap<LoggerState>,
    level: AtomicU8,
    initialized: AtomicBool,
}

impl UnifiedLoggerCore {
    fn new() -> Self {
        Self {
            state: ArcSwap::from_pointee(LoggerState::default_stdout()),
            level: AtomicU8::new(LogLevel::Info as u8),
            initialized: AtomicBool::new(false),
        }
    }

    pub(crate) fn state(&self) -> arc_swap::Guard<Arc<LoggerState>> {
        self.state.load()
    }

    pub(crate) fn set_level_enum(&self, level: LogLevel) {
        self.level.store(level as u8, Ordering::Relaxed);
        log::set_max_level(to_std_log_filter(level));
    }

    pub(crate) fn get_level(&self) -> LogLevel {
        LogLevel::from_u8(self.level.load(Ordering::Relaxed))
    }

    pub(crate) fn is_initialized(&self) -> bool {
        self.initialized.load(Ordering::Acquire)
    }

    /// Encode and emit a record with optional extra bound fields (e.g. from `ContextLogger` or
    /// `slog` `OwnedKVList`).
    pub(crate) fn emit_with_bound(&self, record: &LogRecord<'_>, extra_bound: &[LogField<'_>]) {
        if !self.enabled(record.target, record.level) {
            return;
        }
        let state = self.state.load();
        let mut enc_cfg = state.encoder_cfg;
        if matches!(record.target, LogTarget::ErrVerbose) {
            enc_cfg.force_error_verbose = true;
        }

        let mut bound: Vec<LogField<'_>> =
            Vec::with_capacity(state.bound_fields.len() + extra_bound.len());
        for f in &state.bound_fields {
            bound.push(f.as_field());
        }
        bound.extend_from_slice(extra_bound);

        // TiDB configures zap.AddStacktrace(zapcore.FatalLevel): auto-attach stack only on Fatal
        // if the caller has not already provided an explicit `"stack"` field.
        let auto_stack =
            if record.level == LogLevel::Fatal && !record.fields.iter().any(|f| f.key == "stack") {
                format_backtrace_string(&std::backtrace::Backtrace::force_capture())
            } else {
                String::new()
            };

        let entry = Entry {
            time: encoder::now_local_fixed(),
            level: record.level,
            logger_name: record.logger_name,
            file: record.file,
            line: record.line,
            msg: record.msg,
            bound_fields: &bound,
            fields: record.fields,
            stack: &auto_stack,
        };

        let mut buf = Vec::with_capacity(256);
        match state.format {
            LogFormat::Text => encoder::text::encode_entry(enc_cfg, &entry, &mut buf),
            LogFormat::Json => encoder::json::encode_entry(enc_cfg, &entry, &mut buf),
        }

        let sink: &Arc<dyn LogSink> = match record.target {
            LogTarget::General => &state.general_sink,
            LogTarget::Main | LogTarget::ErrVerbose | LogTarget::Fatal => &state.main_sink,
        };
        let _ = sink.write_entry(&buf);
    }
}

impl StructuredLogger for UnifiedLoggerCore {
    fn enabled(&self, target: LogTarget, level: LogLevel) -> bool {
        match target {
            LogTarget::General | LogTarget::Fatal => true,
            LogTarget::Main | LogTarget::ErrVerbose => {
                (level as u8) >= self.level.load(Ordering::Relaxed)
            }
        }
    }

    fn level(&self) -> LogLevel {
        self.get_level()
    }

    fn log(&self, record: &LogRecord<'_>) {
        self.emit_with_bound(record, &[]);
    }

    fn sync(&self) -> io::Result<()> {
        let state = self.state.load();
        let r1 = state.main_sink.sync();
        let r2 = if !Arc::ptr_eq(&state.main_sink, &state.general_sink) {
            state.general_sink.sync()
        } else {
            Ok(())
        };
        r1.and(r2)
    }
}

static GLOBAL_CORE: std::sync::LazyLock<UnifiedLoggerCore> =
    std::sync::LazyLock::new(UnifiedLoggerCore::new);
static RECONFIGURE_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());
static INSTALL_BRIDGES_ONCE: Once = Once::new();

/// Access the global [`UnifiedLoggerCore`].
pub(crate) fn core() -> &'static UnifiedLoggerCore {
    &GLOBAL_CORE
}

/// Initialize the process-wide unified logger, register it into `tidb_base::structured_log`,
/// install the `log`, `tracing`, and `slog-global` direct bridges (never calling
/// `slog_global::redirect_std_log`), and install the global single-line panic hook.
// go: InitLogger L2 (registers rank-0 StructuredLogger + log/tracing/slog bridges + panic hook)
pub fn init_logger(cfg: &LogConfig, keyspace_name: &str) -> Result<(), Error> {
    init_logger_inner(cfg, keyspace_name, None, None, true)
}

/// Initialize the logger with custom sinks (e.g. [`crate::sink::MemorySink`]) for deterministic
/// tests without installing the process-wide panic hook unless requested.
// glue: deterministic test initializer with injectable sinks
pub fn init_logger_with_sinks(
    cfg: &LogConfig,
    keyspace_name: &str,
    main_sink: Arc<dyn LogSink>,
    general_sink: Option<Arc<dyn LogSink>>,
) -> Result<(), Error> {
    init_logger_inner(cfg, keyspace_name, Some(main_sink), general_sink, false)
}

fn init_logger_inner(
    cfg: &LogConfig,
    keyspace_name: &str,
    override_main: Option<Arc<dyn LogSink>>,
    override_general: Option<Arc<dyn LogSink>>,
    install_panic_hook: bool,
) -> Result<(), Error> {
    let _reconf = RECONFIGURE_LOCK.lock();
    let (new_state, level) =
        LoggerState::from_config(cfg, keyspace_name, override_main, override_general)?;
    let core = core();
    let old = core.state.swap(Arc::new(new_state));
    let _ = old.main_sink.sync();
    if !Arc::ptr_eq(&old.main_sink, &old.general_sink) {
        let _ = old.general_sink.sync();
    }
    core.set_level_enum(level);
    core.initialized.store(true, Ordering::Release);

    INSTALL_BRIDGES_ONCE.call_once(|| {
        let _ = tidb_base::structured_log::register_logger(core);
        bridges::install_all();
    });
    if install_panic_hook {
        panic_hook::set_panic_hook();
    }
    Ok(())
}

/// Dynamically update the global log level from a string (`"debug"`, `"info"`, `"warn"`,
/// `"error"`, `"dpanic"`, `"panic"`, `"fatal"`).
// go: SetLevel L1
pub fn set_level(level: &str) -> Result<(), Error> {
    let parsed = LogLevel::parse_level(level)?;
    core().set_level_enum(parsed);
    Ok(())
}

/// Dynamically update the global log level from a [`LogLevel`] enum.
// glue: typed SetLevel helper (used by printer::print_tidb_info)
pub fn set_level_enum(level: LogLevel) -> Result<(), Error> {
    core().set_level_enum(level);
    Ok(())
}

/// Read the current global log level.
// go: GetLevel L1
pub fn get_level() -> LogLevel {
    core().get_level()
}

/// Returns whether [`init_logger`] (or [`init_logger_with_sinks`]) has been called.
// glue: check before hot-reloading from sysvar hooks
pub fn is_initialized() -> bool {
    core().is_initialized()
}

/// Atomically rebuild and swap the global logger from `cfg`, flushing previous sinks first.
///
/// Because `ErrVerboseLogger` shares the same [`ArcSwap<LoggerState>`] handle, it updates
/// automatically (fixing the Go upstream bug where `ReplaceLogger` did not refresh
/// `errVerboseLogger`).
// go: ReplaceLogger L1
pub fn replace_logger(cfg: &LogConfig) -> Result<(), Error> {
    {
        let _reconf = RECONFIGURE_LOCK.lock();
        let keyspace = core().state().keyspace_name.clone();
        let (new_state, level) = LoggerState::from_config(cfg, &keyspace, None, None)?;
        let old = core().state.swap(Arc::new(new_state));
        let _ = old.main_sink.sync();
        if !Arc::ptr_eq(&old.main_sink, &old.general_sink) {
            let _ = old.general_sink.sync();
        }
        core().set_level_enum(level);
    }

    // Go pkg/util/logutil/log.go:199-214 marshals pingcap/log.Config in struct field order and
    // logs `replaced global logger with config: <json>`.
    let cfg_json = marshal_log_config_struct_order(cfg);
    let msg = format!("replaced global logger with config: {cfg_json}");
    // log-skip: Go pkg/util/logutil/log.go:214 intentionally embeds config JSON in message string
    tidb_base::log_info!(&msg);
    Ok(())
}

fn marshal_log_config_struct_order(cfg: &LogConfig) -> String {
    use std::fmt::Write as _;
    use tidb_base::gostd::json::append_string;
    let mut out = String::with_capacity(256);
    out.push_str("{\"level\":");
    append_string(&mut out, &cfg.level);
    out.push_str(",\"format\":");
    append_string(&mut out, &cfg.format);
    let _ = write!(out, ",\"disable-timestamp\":{}", cfg.disable_timestamp);
    out.push_str(",\"file\":{\"filename\":");
    append_string(&mut out, &cfg.file.filename);
    let _ = write!(
        out,
        ",\"max-size\":{},\"max-days\":{},\"max-backups\":{},\"compression\":",
        cfg.file.max_size, cfg.file.max_days, cfg.file.max_backups
    );
    append_string(&mut out, &cfg.file.compression);
    let _ = write!(
        out,
        ",\"is-buffered\":{},\"buffer-size\":{},\"buffer-flush-interval\":{}}}",
        cfg.file.is_buffered, cfg.file.buffer_size, cfg.file.buffer_flush_interval
    );
    let _ = write!(
        out,
        ",\"disable-error-verbose\":{},\"timeout\":{},\"slow-query-file\":",
        cfg.disable_error_verbose, cfg.timeout
    );
    append_string(&mut out, &cfg.slow_query_file);
    out.push_str(",\"general-log-file\":");
    append_string(&mut out, &cfg.general_log_file);
    out.push('}');
    out
}

/// Flush all active log sinks.
// glue: log.Sync() at graceful shutdown and fatal paths
pub fn sync() -> io::Result<()> {
    core().sync()
}

/// Reserved slow-query sink accessor for `D14.2` (`tidb-observe`).
// glue: D14.2 slow_query_log sink reservation
pub fn slow_query_sink() -> Option<Arc<dyn LogSink>> {
    core().state().slow_query_sink.clone()
}

/// Per-connection / per-task context logger carrying Go `logutil.Logger(ctx)` bound fields
/// (`conn`, `session_alias`, or custom `.With(...)` fields).
#[derive(Clone, Debug, Default)]
pub struct ContextLogger {
    bound: Vec<OwnedLogField>,
    target: LogTargetProxy,
}

#[derive(Clone, Copy, Debug, Default)]
enum LogTargetProxy {
    #[default]
    Main,
    ErrVerbose,
    General,
}

impl LogTargetProxy {
    fn as_target(self) -> LogTarget {
        match self {
            LogTargetProxy::Main => LogTarget::Main,
            LogTargetProxy::ErrVerbose => LogTarget::ErrVerbose,
            LogTargetProxy::General => LogTarget::General,
        }
    }
}

impl ContextLogger {
    /// Background logger (`logutil.BgLogger()`).
    pub fn bg() -> Self {
        Self::default()
    }

    /// Error-verbose logger (`logutil.ErrVerboseLogger()`), always sharing the active global
    /// `ArcSwap<LoggerState>` handle.
    pub fn err_verbose() -> Self {
        Self {
            bound: Vec::new(),
            target: LogTargetProxy::ErrVerbose,
        }
    }

    /// General query logger (`logutil.GeneralLogger`).
    pub fn general() -> Self {
        Self {
            bound: Vec::new(),
            target: LogTargetProxy::General,
        }
    }

    /// Connection-scoped logger (`logutil.WithConnID` + `session_alias`).
    pub fn with_conn(conn_id: u64, session_alias: impl Into<String>) -> Self {
        Self {
            bound: vec![
                OwnedLogField::u64("conn", conn_id),
                OwnedLogField::string("session_alias", session_alias),
            ],
            target: LogTargetProxy::Main,
        }
    }

    /// Append a bound field (`.With(...)`) preserving declaration order.
    pub fn with_field(mut self, field: OwnedLogField) -> Self {
        self.bound.push(field);
        self
    }

    /// Emit a structured log entry with this logger's bound fields prepended before `fields`.
    pub fn emit(&self, level: LogLevel, file: &str, line: u32, msg: &str, fields: &[LogField<'_>]) {
        let extra: Vec<LogField<'_>> = self.bound.iter().map(OwnedLogField::as_field).collect();
        core().emit_with_bound(
            &LogRecord {
                target: self.target.as_target(),
                level,
                logger_name: "",
                file,
                line,
                msg,
                fields,
            },
            &extra,
        );
    }
}

fn to_std_log_filter(level: LogLevel) -> log::LevelFilter {
    match level {
        LogLevel::Debug => log::LevelFilter::Debug,
        LogLevel::Info => log::LevelFilter::Info,
        LogLevel::Warn => log::LevelFilter::Warn,
        LogLevel::Error | LogLevel::DPanic | LogLevel::Panic | LogLevel::Fatal => {
            log::LevelFilter::Error
        }
    }
}
