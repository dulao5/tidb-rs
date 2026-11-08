//! go-file: pkg/config/config.go
//!
//! Port of Go TiDB v8.5.8 `pkg/config` (block D00.2): the configuration of tidb-server,
//! loaded from TOML (`Config::load`, through the BurntSushi port in [`crate::gotoml`]),
//! validated (`Config::valid`) and kept in a process-wide global (`get_global_config`).
//!
//! The struct types and their defaults are generated from the Go types and from the run-time
//! value of `config.NewConfig()` (`types_gen.rs`, `defaults_gen.rs`). Embedded client-go,
//! logutil and jaeger types are ported as struct shapes and defaults only; the parts of their
//! packages that `config.go` calls are boundary copies here (see `specs/D00.2-base/plan.md`).

// Package-level constants and vars keep their Go names (mapping rule M-GONAME).
#![allow(non_upper_case_globals)]

mod config_util;
mod defaults_gen;
pub mod reflect;
mod types_gen;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool as StdAtomicBool, Ordering};
use std::sync::{Arc, LazyLock};

use arc_swap::{ArcSwap, Guard};
use parking_lot::Mutex;

pub use config_util::*;
pub use reflect::{
    AtomicBool, Duration, FieldDesc, GoStruct, NullableBool, ValMut, ValRef, nbFalse, nbTrue,
    nbUnset,
};
pub use types_gen::*;

use crate::errors::{self, Error};
use crate::gofmt::Arg;
use crate::gostd::json;
use crate::gotoml;

// Config number limitations
pub const MaxLogFileSize: isize = 4096; // MB
/// MaxTxnEntrySize is the max value of TxnEntrySizeLimit.
pub const MaxTxnEntrySizeLimit: u64 = 120 * 1024 * 1024; // 120MB
/// MaxPluginAuditLogBufferSize is the max buffer size for plugin audit log.
pub const MaxPluginAuditLogBufferSize: isize = 100 * 1024 * 1024;
/// MaxPluginAuditLogFlushInterval is the max time interval to flush plugin audit log.
pub const MaxPluginAuditLogFlushInterval: isize = 3600;
/// DefTxnEntrySizeLimit is the default value of TxnEntrySizeLimit.
pub const DefTxnEntrySizeLimit: u64 = 6 * 1024 * 1024;
/// DefTxnTotalSizeLimit is the default value of TxnTxnTotalSizeLimit.
pub const DefTxnTotalSizeLimit: u64 = 100 * 1024 * 1024;
/// 100T, we expect a txn can never be this large
pub const SuperLargeTxnSize: u64 = 100 * 1024 * 1024 * 1024 * 1024;
/// DefMaxIndexLength is the maximum index length(in bytes). This value is consistent with MySQL.
pub const DefMaxIndexLength: isize = 3072;
/// DefMaxOfMaxIndexLength is the maximum index length(in bytes) for TiDB v3.0.7 and previous version.
pub const DefMaxOfMaxIndexLength: isize = 3072 * 4;
/// DefIndexLimit is the limitation of index on a single table. This value is consistent with MySQL.
pub const DefIndexLimit: isize = 64;
/// DefMaxOfIndexLimit is the maximum limitation of index on a single table for TiDB.
pub const DefMaxOfIndexLimit: isize = 64 * 8;
/// DefPort is the default port of TiDB
pub const DefPort: usize = 4000;
/// DefStatusPort is the default status port of TiDB
pub const DefStatusPort: usize = 10080;
/// DefHost is the default host of TiDB
pub const DefHost: &str = "0.0.0.0";
/// DefStatusHost is the default status host of TiDB
pub const DefStatusHost: &str = "0.0.0.0";
/// DefTableColumnCountLimit is limit of the number of columns in a table
pub const DefTableColumnCountLimit: u32 = 1017;
/// DefMaxOfTableColumnCountLimit is maximum limitation of the number of columns in a table
pub const DefMaxOfTableColumnCountLimit: u32 = 4096;
/// DefStatsLoadConcurrencyLimit is limit of the concurrency of stats-load. When it is set to 0, it will be set by syncload.GetSyncLoadConcurrencyByCPU.
pub const DefStatsLoadConcurrencyLimit: isize = 0;
/// DefMaxOfStatsLoadConcurrencyLimit is maximum limitation of the concurrency of stats-load
pub const DefMaxOfStatsLoadConcurrencyLimit: isize = 128;
/// DefStatsLoadQueueSizeLimit is limit of the size of stats-load request queue
pub const DefStatsLoadQueueSizeLimit: usize = 1;
/// DefMaxOfStatsLoadQueueSizeLimit is maximum limitation of the size of stats-load request queue
pub const DefMaxOfStatsLoadQueueSizeLimit: usize = 100000;
/// DefDDLSlowOprThreshold sets log DDL operations whose execution time exceeds the threshold value.
pub const DefDDLSlowOprThreshold: u32 = 300;
/// DefExpensiveQueryTimeThreshold indicates the time threshold of expensive query.
pub const DefExpensiveQueryTimeThreshold: u64 = 60;
/// DefExpensiveTxnTimeThreshold indicates the time threshold of expensive txn.
pub const DefExpensiveTxnTimeThreshold: u64 = 600;
/// DefMemoryUsageAlarmRatio is the threshold triggering an alarm which the memory usage of tidb-server instance exceeds.
pub const DefMemoryUsageAlarmRatio: f64 = 0.8;
/// DefTempDir is the default temporary directory path for TiDB.
pub const DefTempDir: &str = "/tmp/tidb";
/// DefPluginAuditLogBufferSize is the default buffer size for plugin audit log.
pub const DefPluginAuditLogBufferSize: isize = 0;
/// DefPluginAuditLogFlushInterval is the default time interval to flush plugin audit log.
pub const DefPluginAuditLogFlushInterval: isize = 30;
/// DefAuthTokenRefreshInterval is the default time interval to refresh tidb auth token.
pub const DefAuthTokenRefreshInterval: Duration = Duration(3_600_000_000_000);
/// EnvVarKeyspaceName is the system env name for keyspace name.
pub const EnvVarKeyspaceName: &str = "KEYSPACE_NAME";
/// MaxTokenLimit is the max token limit value.
pub const MaxTokenLimit: usize = 1024 * 1024;
pub const DefSchemaLease: Duration = Duration(45_000_000_000);
/// DefRowsForSampleRate is default sample rows used to calculate samplerate (const.go).
pub const DefRowsForSampleRate: isize = 110000;

/// The following constants represents the valid action configurations for
/// Security.SpilledFileEncryptionMethod. "plaintext" means encryption is disabled.
pub const SpilledFileEncryptionMethodPlaintext: &str = "plaintext";
pub const SpilledFileEncryptionMethodAES128CTR: &str = "aes128-ctr";

/// Valid config maps. (Go: a mutable package var; iteration order is random in Go, sorted
/// here.)
pub static ValidStorage: LazyLock<BTreeMap<&'static str, bool>> =
    LazyLock::new(|| BTreeMap::from([("mocktikv", true), ("tikv", true), ("unistore", true)]));
/// CheckTableBeforeDrop enable to execute `admin check table` before `drop table`.
pub static CheckTableBeforeDrop: StdAtomicBool = StdAtomicBool::new(false);
/// checkBeforeDropLDFlag is a go build flag.
const checkBeforeDropLDFlag: &str = "None";
/// tempStorageDirName is the default temporary storage dir name by base64 encoding a string
/// `port/statusPort`
static tempStorageDirName: LazyLock<String> = LazyLock::new(|| {
    encode_def_temp_storage_dir(
        &os_temp_dir(),
        DefHost,
        DefStatusHost,
        DefPort,
        DefStatusPort,
    )
});

/// `tempStorageDirName` (computed from the environment at start-up, like Go's package var).
// glue: read access to the package var tempStorageDirName
pub fn temp_storage_dir_name() -> String {
    tempStorageDirName.clone()
}

/// `os.TempDir` on Unix: `$TMPDIR`, or `/tmp`.
// glue: os.TempDir (Unix)
fn os_temp_dir() -> String {
    match std::env::var("TMPDIR") {
        Ok(d) if !d.is_empty() => d,
        _ => "/tmp".to_owned(),
    }
}

/// InstanceConfigSection indicates a config session that has options moved to `[instance]`
/// session.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InstanceConfigSection {
    /// SectionName indicates the origin section name.
    pub section_name: String,
    /// NameMappings maps the origin name to the name in `[instance]`. (Go map: random
    /// iteration order; sorted here.)
    pub name_mappings: BTreeMap<String, String>,
}

/// sectionMovedToInstance records all config section and options that should be moved to
/// `[instance]`.
static sectionMovedToInstance: LazyLock<Vec<InstanceConfigSection>> = LazyLock::new(|| {
    let sec = |name: &str, m: &[(&str, &str)]| InstanceConfigSection {
        section_name: name.to_owned(),
        name_mappings: m
            .iter()
            .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
            .collect(),
    };
    vec![
        sec(
            "",
            &[
                ("check-mb4-value-in-utf8", "tidb_check_mb4_value_in_utf8"),
                (
                    "enable-collect-execution-info",
                    "tidb_enable_collect_execution_info",
                ),
                ("max-server-connections", "max_connections"),
                ("run-ddl", "tidb_enable_ddl"),
            ],
        ),
        sec(
            "log",
            &[
                ("enable-slow-log", "tidb_enable_slow_log"),
                ("slow-threshold", "tidb_slow_log_threshold"),
                ("record-plan-in-slow-log", "tidb_record_plan_in_slow_log"),
            ],
        ),
        sec(
            "performance",
            &[
                ("force-priority", "tidb_force_priority"),
                ("memory-usage-alarm-ratio", "tidb_memory_usage_alarm_ratio"),
            ],
        ),
        sec("plugin", &[("load", "plugin_load"), ("dir", "plugin_dir")]),
    ]
});

/// ConflictOptions indicates the conflict config options existing in both `[instance]` and
/// other sections in config file.
pub static ConflictOptions: Mutex<Vec<InstanceConfigSection>> = Mutex::new(Vec::new());

/// DeprecatedOptions indicates the config options existing in some other sections in config
/// file. They should be moved to `[instance]` section.
pub static DeprecatedOptions: Mutex<Vec<InstanceConfigSection>> = Mutex::new(Vec::new());

/// TikvConfigLock protects against concurrent tikv config refresh
pub static TikvConfigLock: Mutex<()> = Mutex::new(());

/// The errors of [`Config::load`]: Go callers type-assert `*ErrConfigValidationFailed` and
/// `*ErrConfigInstanceSection` (`InitializeConfig`), so they stay distinct types (M-ERR keeps
/// one carrier type for everything else).
#[derive(Clone, Debug)]
pub enum LoadError {
    /// `*ErrConfigValidationFailed`.
    ValidationFailed(ErrConfigValidationFailed),
    /// `*ErrConfigInstanceSection`.
    InstanceSection(ErrConfigInstanceSection),
    /// Any other error (TOML syntax and type errors, I/O).
    Other(Error),
}

impl LoadError {
    /// `Error()` of the Go error value.
    // glue: dynamic dispatch of Error() over the Go error types of Load
    pub fn error(&self) -> Vec<u8> {
        match self {
            LoadError::ValidationFailed(e) => e.error().into_bytes(),
            LoadError::InstanceSection(e) => e.error().into_bytes(),
            LoadError::Other(e) => e.error(),
        }
    }
}

/// The ErrConfigValidationFailed error is used so that external callers can do a type
/// assertion to defer handling of this specific error when someone does not want strict
/// type checking. This is needed only because logging hasn't been set up at the time we
/// parse the config file. This should all be ripped out once strict config checking is made
/// the default behavior.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ErrConfigValidationFailed {
    conf_file: String,
    pub undecoded_items: Vec<String>,
}

impl ErrConfigValidationFailed {
    // go: ErrConfigValidationFailed.Error L1
    pub fn error(&self) -> String {
        format!(
            "config file {} contained invalid configuration options: {}; check TiDB manual to make sure this option has not been deprecated and removed from your TiDB version if the option does not appear to be a typo",
            self.conf_file,
            self.undecoded_items.join(", ")
        )
    }
}

/// ErrConfigInstanceSection error is used to warning the user which config options should be
/// moved to 'instance'. (Go keeps pointers to `ConflictOptions` and `DeprecatedOptions`;
/// `error` reads the globals.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ErrConfigInstanceSection {
    conf_file: String,
}

impl ErrConfigInstanceSection {
    // go: ErrConfigInstanceSection.Error L1
    pub fn error(&self) -> String {
        let config_sections = ConflictOptions.lock().clone();
        let deprecated_sections = DeprecatedOptions.lock().clone();
        let mut builder = String::new();
        if !config_sections.is_empty() {
            builder.push_str("Conflict configuration options exists on both [instance] section and some other sections. ");
        }
        if !deprecated_sections.is_empty() {
            builder.push_str("Some configuration options should be moved to [instance] section. ");
        }
        builder.push_str("Please use the latter config options in [instance] instead: ");
        for config_section in config_sections.iter().chain(deprecated_sections.iter()) {
            for (old_name, new_name) in &config_section.name_mappings {
                builder.push_str(&format!(" ({old_name}, {new_name})"));
            }
        }
        builder.push('.');
        builder
    }
}

impl Config {
    /// UpdateTempStoragePath is to update the `TempStoragePath` if port/statusPort was
    /// changed and the `tmp-storage-path` was not specified in the conf.toml or was specified
    /// the same as the default value.
    // go: Config.UpdateTempStoragePath L1
    pub fn update_temp_storage_path(&mut self) {
        if self.temp_storage_path == *tempStorageDirName {
            self.temp_storage_path = encode_def_temp_storage_dir(
                &os_temp_dir(),
                &self.host,
                &self.status.status_host,
                self.port,
                self.status.status_port,
            );
        } else {
            self.temp_storage_path = encode_def_temp_storage_dir(
                &self.temp_storage_path,
                &self.host,
                &self.status.status_host,
                self.port,
                self.status.status_port,
            );
        }
    }

    /// GetTiKVConfig returns configuration options from tikvcfg
    // go: Config.GetTiKVConfig L1
    pub fn get_tikv_config(&self) -> TikvConfig {
        TikvConfig {
            committer_concurrency: crate::tikvutil::CommitterConcurrency.load(Ordering::SeqCst)
                as isize,
            max_txn_ttl: self.performance.max_txn_ttl,
            tikv_client: self.tikv_client.clone(),
            security: self.security.cluster_security(),
            pd_client: self.pd_client.clone(),
            pessimistic_txn: TikvPessimisticTxn {
                max_retry_count: self.pessimistic_txn.max_retry_count,
            },
            txn_local_latches: self.txn_local_latches.clone(),
            stores_refresh_interval: self.stores_refresh_interval,
            open_tracing_enable: self.open_tracing.enable,
            path: self.path.clone(),
            enable_forwarding: self.enable_forwarding,
            txn_scope: self
                .labels
                .as_ref()
                .and_then(|l| l.get("zone").cloned())
                .unwrap_or_default(),
            enable_async_batch_get: self.performance.enable_async_batch_get,
            ..TikvConfig::default()
        }
    }
}

// go: encodeDefTempStorageDir L1
fn encode_def_temp_storage_dir(
    temp_dir: &str,
    host: &str,
    status_host: &str,
    port: usize,
    status_port: usize,
) -> String {
    let dir_name = crate::gostd::path::base64_url_encode(
        format!("{host}:{port}/{status_host}:{status_port}").as_bytes(),
    );
    // user.Current().Uid on Unix is the real user ID.
    let os_uid = rustix::process::getuid().as_raw().to_string();
    crate::gostd::path::join(&[
        temp_dir,
        &format!("{os_uid}_tidb"),
        &dir_name,
        "tmp-storage",
    ])
}

impl NullableBool {
    // go: nullableBool.toBool L1
    pub fn to_bool(&self) -> bool {
        self.is_valid && self.is_true
    }

    // go: nullableBool.MarshalJSON L1
    pub fn marshal_json(&self) -> Vec<u8> {
        match *self {
            nbTrue => b"true".to_vec(),
            nbFalse => b"false".to_vec(),
            _ => b"null".to_vec(),
        }
    }

    // go: nullableBool.UnmarshalText L1
    pub fn unmarshal_text(&mut self, text: &[u8]) -> Result<(), Error> {
        match text {
            b"" | b"null" => {
                *self = nbUnset;
                return Ok(());
            }
            b"true" => *self = nbTrue,
            b"false" => *self = nbFalse,
            _ => {
                *self = nbUnset;
                let mut msg = b"Invalid value for bool type: ".to_vec();
                msg.extend_from_slice(text);
                return Err(errors::new(msg));
            }
        }
        Ok(())
    }

    // go: nullableBool.MarshalText L1
    pub fn marshal_text(&self) -> Vec<u8> {
        if !self.is_valid {
            return Vec::new();
        }
        if self.is_true {
            return b"true".to_vec();
        }
        b"false".to_vec()
    }

    // go: nullableBool.UnmarshalJSON L1
    pub fn unmarshal_json(&mut self, data: &[u8]) -> Result<(), Error> {
        let v = json::unmarshal(data).map_err(errors::new)?;
        match v {
            json::Value::Bool(raw) => {
                *self = NullableBool {
                    is_valid: true,
                    is_true: raw,
                }
            }
            _ => *self = nbUnset,
        }
        Ok(())
    }
}

/// NewAtomicBool creates an AtomicBool.
// go: NewAtomicBool L1
pub fn new_atomic_bool(v: bool) -> AtomicBool {
    AtomicBool::new(v)
}

impl AtomicBool {
    /// MarshalText implements the encoding.TextMarshaler interface.
    // go: AtomicBool.MarshalText L1
    pub fn marshal_text(&self) -> Vec<u8> {
        if self.load() {
            return b"true".to_vec();
        }
        b"false".to_vec()
    }

    /// UnmarshalText implements the encoding.TextUnmarshaler interface.
    // go: AtomicBool.UnmarshalText L1
    pub fn unmarshal_text(&mut self, text: &[u8]) -> Result<(), Error> {
        match text {
            b"" | b"null" => *self = AtomicBool::new(false),
            b"true" => *self = AtomicBool::new(true),
            b"false" => *self = AtomicBool::new(false),
            _ => {
                *self = AtomicBool::new(false);
                let mut msg = b"Invalid value for bool type: ".to_vec();
                msg.extend_from_slice(text);
                return Err(errors::new(msg));
            }
        }
        Ok(())
    }
}

impl Log {
    // go: Log.getDisableTimestamp L1
    fn get_disable_timestamp(&self) -> bool {
        if self.enable_timestamp == nbUnset && self.disable_timestamp == nbUnset {
            return false;
        }
        if self.enable_timestamp == nbUnset {
            return self.disable_timestamp.to_bool();
        }
        !self.enable_timestamp.to_bool()
    }

    // go: Log.getDisableErrorStack L1
    fn get_disable_error_stack(&self) -> bool {
        if self.enable_error_stack == nbUnset && self.disable_error_stack == nbUnset {
            return true;
        }
        if self.enable_error_stack == nbUnset {
            return self.disable_error_stack.to_bool();
        }
        !self.enable_error_stack.to_bool()
    }
}

impl Security {
    /// ClusterSecurity returns Security info for cluster
    // go: Security.ClusterSecurity L1
    pub fn cluster_security(&self) -> TikvSecurity {
        // tikvcfg.NewSecurity
        TikvSecurity {
            cluster_ssl_ca: self.cluster_ssl_ca.clone(),
            cluster_ssl_cert: self.cluster_ssl_cert.clone(),
            cluster_ssl_key: self.cluster_ssl_key.clone(),
            cluster_verify_cn: self.cluster_verify_cn.clone(),
        }
    }
}

impl TrxSummary {
    /// Valid Validatse TrxSummary configs
    // go: TrxSummary.Valid L1
    pub fn valid(&self) -> Result<(), Error> {
        if self.transaction_summary_capacity > 5000 {
            return Err(errors::new(
                "transaction-summary.transaction-summary-capacity should not be larger than 5000",
            ));
        }
        Ok(())
    }
}

/// DefaultPessimisticTxn returns the default configuration for PessimisticTxn
// go: DefaultPessimisticTxn L1
pub fn default_pessimistic_txn() -> PessimisticTxn {
    PessimisticTxn {
        max_retry_count: 256,
        deadlock_history_capacity: 10,
        deadlock_history_collect_retryable: false,
        pessimistic_auto_commit: new_atomic_bool(false),
        constraint_check_in_place_pessimistic: true,
    }
}

/// DefaultTrxSummary returns the default configuration for TrxSummary collector
// go: DefaultTrxSummary L1
pub fn default_trx_summary() -> TrxSummary {
    // TrxSummary is not enabled by default before GA
    TrxSummary {
        transaction_summary_capacity: 500,
        transaction_id_digest_min_duration: 2147483647,
    }
}

/// The process-wide configs: `globalConf` of this package and the global config of client-go
/// (`tikvcfg.StoreGlobalConfig`, stored by `StoreGlobalConfig`). Initialised on first use by
/// `init` (Go: at package initialisation).
/// Both are published snapshots (Go `atomic.Pointer`): `ArcSwap` (mapping rule M-SNAP), so
/// that the hot `GetGlobalConfig` does not write shared cache lines.
struct Globals {
    conf: ArcSwap<Config>,
    tikv: ArcSwap<TikvConfig>,
}

static GLOBALS: LazyLock<Globals> = LazyLock::new(|| {
    let g = Globals {
        conf: ArcSwap::from_pointee(defaults_gen::default_conf()),
        tikv: ArcSwap::from_pointee(defaults_gen::tikv_default_config()),
    };
    init(&g);
    g
});

/// NewConfig creates a new config instance with default value.
// go: NewConfig L1
pub fn new_config() -> Config {
    defaults_gen::default_conf()
}

/// GetGlobalConfig returns the global configuration for this server. It should store
/// configuration from command line and configuration file. Other parts of the system can
/// read the global configuration use this function.
///
/// The guard derefs to the config (`get_global_config().port`); it is meant for short
/// reads. Keep a snapshot with `Guard::into_inner` / [`get_global_config_arc`].
// go: GetGlobalConfig L1 [M-SNAP]
pub fn get_global_config() -> Guard<Arc<Config>> {
    GLOBALS.conf.load()
}

/// [`get_global_config`] as an owned snapshot (Go: keeping the `*Config`).
// glue: ArcSwap::load_full (M-SNAP)
pub fn get_global_config_arc() -> Arc<Config> {
    GLOBALS.conf.load_full()
}

/// StoreGlobalConfig stores a new config to the globalConf. It mostly uses in the test to
/// avoid some data races.
// go: StoreGlobalConfig L1
pub fn store_global_config(config: Arc<Config>) {
    store_global_config_in(&GLOBALS, config);
}

// glue: StoreGlobalConfig on the globals being initialised (Go's init runs before any use)
fn store_global_config_in(g: &Globals, config: Arc<Config>) {
    let cfg = config.get_tikv_config();
    g.conf.store(config);
    let _guard = TikvConfigLock.lock();
    g.tikv.store(Arc::new(cfg));
}

/// `tikvcfg.GetGlobalConfig` (client-go's global, stored by [`store_global_config`]).
// glue: boundary of client-go config.GetGlobalConfig
pub fn tikv_get_global_config() -> Guard<Arc<TikvConfig>> {
    GLOBALS.tikv.load()
}

/// removedConfig contains items that are no longer supported. they might still be in the
/// config struct to support import, but are not actively used.
static removedConfig: &[&str] = &[
    "pessimistic-txn.ttl",
    "pessimistic-txn.enable",
    "log.file.log-rotate",
    "log.log-slow-query",
    "txn-local-latches",
    "txn-local-latches.enabled",
    "txn-local-latches.capacity",
    "performance.max-memory",
    "max-txn-time-use",
    "experimental.allow-auto-random",
    "enable-redact-log", // use variable tidb_redact_log instead
    "enable-streaming",
    "performance.mem-profile-interval",
    "security.require-secure-transport",
    "lower-case-table-names",
    "stmt-summary",
    "stmt-summary.enable",
    "stmt-summary.enable-internal-query",
    "stmt-summary.max-stmt-count",
    "stmt-summary.max-sql-length",
    "stmt-summary.refresh-interval",
    "stmt-summary.history-size",
    "enable-batch-dml", // use tidb_enable_batch_dml
    "mem-quota-query",
    "log.query-log-max-len",
    "performance.committer-concurrency",
    "experimental.enable-global-kill",
    "performance.run-auto-analyze", // use tidb_enable_auto_analyze
    // use tidb_enable_prepared_plan_cache, tidb_prepared_plan_cache_size and tidb_prepared_plan_cache_memory_guard_ratio
    "prepared-plan-cache.enabled",
    "prepared-plan-cache.capacity",
    "prepared-plan-cache.memory-guard-ratio",
    "oom-action",
    "check-mb4-value-in-utf8",       // use tidb_check_mb4_value_in_utf8
    "enable-collect-execution-info", // use tidb_enable_collect_execution_info
    "log.enable-slow-log",           // use tidb_enable_slow_log
    "log.slow-threshold",            // use tidb_slow_log_threshold
    "log.record-plan-in-slow-log",   // use tidb_record_plan_in_slow_log
    "log.expensive-threshold",
    "performance.force-priority",           // use tidb_force_priority
    "performance.memory-usage-alarm-ratio", // use tidb_memory_usage_alarm_ratio
    "plugin.load",                          // use plugin_load
    "plugin.dir",                           // use plugin_dir
    "performance.feedback-probability",     // This feature is deprecated
    "performance.query-feedback-limit",
    "oom-use-tmp-storage",    // use tidb_enable_tmp_storage_on_oom
    "max-server-connections", // use sysvar max_connections
    "run-ddl",                // use sysvar tidb_enable_ddl
    "instance.tidb_memory_usage_alarm_ratio", // use sysvar tidb_memory_usage_alarm_ratio
    "enable-global-index",    // use sysvar tidb_enable_global_index
];

/// isAllRemovedConfigItems returns true if all the items that couldn't validate belong to
/// the list of removedConfig items.
// go: isAllRemovedConfigItems L1
fn is_all_removed_config_items(items: &[String]) -> bool {
    items
        .iter()
        .all(|item| removedConfig.contains(&item.as_str()))
}

/// The outcome of [`initialize_config`] where Go calls `os.Exit` (the caller exits).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InitializeExit {
    /// `os.Exit(code)` after printing to stderr / stdout.
    Exit(i32),
}

/// InitializeConfig initialize the global config handler. The function enforceCmdArgs is
/// used to merge the config file with command arguments: For example, if you start TiDB by
/// the command "./tidb-server --port=3000", the port number should be overwritten to 3000
/// and ignore the port number in the config file.
///
/// Go calls `os.Exit` (and `terror.MustNil` panics); the port returns
/// [`InitializeExit::Exit`] so that the binary exits, and panics where Go panics.
// go: InitializeConfig L2 (os.Exit is returned to the caller; the flag set is the caller's closure)
pub fn initialize_config(
    conf_path: &str,
    config_check: bool,
    config_strict: bool,
    enforce_cmd_args: impl FnOnce(&mut Config),
) -> Result<(), InitializeExit> {
    let mut cfg = Config::clone(&get_global_config());
    if !conf_path.is_empty() {
        let mut err: Option<Vec<u8>> = None;
        if let Err(e) = cfg.load(conf_path) {
            match &e {
                // Unused config item error turns to warnings.
                LoadError::ValidationFailed(tmp) => {
                    // This block is to accommodate an interim situation where strict config
                    // checking is not the default behavior of TiDB. The warning message
                    // must be deferred until logging has been set up. After strict config
                    // checking is the default behavior, This should all be removed.
                    if (!config_check && !config_strict)
                        || is_all_removed_config_items(&tmp.undecoded_items)
                    {
                        eprintln!("{}", tmp.error());
                    } else {
                        err = Some(e.error());
                    }
                }
                LoadError::InstanceSection(tmp) => log::warn!("{}", tmp.error()),
                LoadError::Other(_) => err = Some(e.error()),
            }
        }
        // In configCheck we always print out which options in the config file have been
        // removed. This helps users upgrade better.
        if config_check {
            if let Err(e) = cfg.removed_variable_check(conf_path) {
                log::warn!("{}", String::from_utf8_lossy(&e.error()));
            }
            err = None; // treat as warning
        }
        // terror.MustNil(err)
        if let Some(e) = err {
            panic!("{}", String::from_utf8_lossy(&e));
        }
    } else {
        // configCheck should have the config file specified.
        if config_check {
            eprintln!("config check failed no config file specified for config-check");
            return Err(InitializeExit::Exit(1));
        }
    }
    enforce_cmd_args(&mut cfg);

    if let Err(e) = cfg.valid() {
        let mut conf_path = conf_path.to_owned();
        if !conf_path.starts_with('/')
            && let Ok(cwd) = std::env::current_dir()
        {
            conf_path = crate::gostd::path::join(&[&cwd.to_string_lossy(), &conf_path]);
        }
        eprintln!("load config file: {conf_path}");
        eprintln!("invalid config {}", String::from_utf8_lossy(&e.error()));
        return Err(InitializeExit::Exit(1));
    }
    if config_check {
        println!("config check successful");
        return Err(InitializeExit::Exit(0));
    }
    store_global_config(Arc::new(cfg));
    Ok(())
}

impl Config {
    /// RemovedVariableCheck checks if the config file contains any items which have been
    /// removed. These will not take effect any more.
    // go: Config.RemovedVariableCheck L1
    pub fn removed_variable_check(&mut self, conf_file: &str) -> Result<(), Error> {
        let (meta_data, r) = gotoml::decode_file(conf_file, self);
        if let Err(e) = r {
            return Err(Error::plain(e.error()));
        }
        let mut removed: Vec<&str> = Vec::new();
        for item in removedConfig {
            // We need to split the string to account for the top level and the section
            // hierarchy of config.
            let tmp: Vec<&str> = item.split('.').collect();
            if (tmp.len() == 2 && meta_data.is_defined(&[tmp[0], tmp[1]]))
                || (tmp.len() == 1 && meta_data.is_defined(&[tmp[0]]))
            {
                removed.push(item);
            }
        }
        if !removed.is_empty() {
            removed.sort_unstable(); // deterministic for tests
            return Err(Error::plain(format!(
                "The following configuration options are no longer supported in this version of TiDB. Check the release notes for more information: {}",
                removed.join(", ")
            )));
        }
        Ok(())
    }

    /// Load loads config options from a toml file.
    // go: Config.Load L1
    pub fn load(&mut self, conf_file: &str) -> Result<(), LoadError> {
        let (meta_data, r) = gotoml::decode_file(conf_file, self);
        self.load_meta(conf_file, meta_data, r)
    }

    /// [`Config::load`] of TOML text (the file named `conf_file` holds `data`).
    // glue: Load with the file content given (tests and vectors)
    pub fn load_data(&mut self, conf_file: &str, data: &[u8]) -> Result<(), LoadError> {
        let (meta_data, r) = gotoml::decode(data, self);
        self.load_meta(conf_file, meta_data, r)
    }

    // glue: the part of Load after toml.DecodeFile
    fn load_meta(
        &mut self,
        conf_file: &str,
        meta_data: gotoml::MetaData,
        r: Result<(), gotoml::DecodeError>,
    ) -> Result<(), LoadError> {
        if let Err(e) = r {
            return Err(LoadError::Other(Error::plain(e.error())));
        }
        if self.token_limit == 0 {
            self.token_limit = 1000;
        } else if self.token_limit > MaxTokenLimit {
            self.token_limit = MaxTokenLimit;
        }
        let mut err: Option<LoadError> = None;
        // If any items in confFile file are not mapped into the Config struct, issue an
        // error and stop the server from starting.
        let undecoded = meta_data.undecoded();
        if !undecoded.is_empty() {
            let undecoded_items = undecoded.iter().map(|k| gotoml::key_string(k)).collect();
            err = Some(LoadError::ValidationFailed(ErrConfigValidationFailed {
                conf_file: conf_file.to_owned(),
                undecoded_items,
            }));
        }

        for section in sectionMovedToInstance.iter() {
            let mut new_conflict_section = InstanceConfigSection {
                section_name: section.section_name.clone(),
                name_mappings: BTreeMap::new(),
            };
            let mut new_deprecated_section = InstanceConfigSection {
                section_name: section.section_name.clone(),
                name_mappings: BTreeMap::new(),
            };
            for (old_name, new_name) in &section.name_mappings {
                if (section.section_name.is_empty() && meta_data.is_defined(&[old_name]))
                    || (!section.section_name.is_empty()
                        && meta_data.is_defined(&[&section.section_name, old_name]))
                {
                    if meta_data.is_defined(&["instance", new_name]) {
                        new_conflict_section
                            .name_mappings
                            .insert(old_name.clone(), new_name.clone());
                    } else {
                        new_deprecated_section
                            .name_mappings
                            .insert(old_name.clone(), new_name.clone());
                    }
                }
            }
            if !new_conflict_section.name_mappings.is_empty() {
                ConflictOptions.lock().push(new_conflict_section);
            }
            if !new_deprecated_section.name_mappings.is_empty() {
                DeprecatedOptions.lock().push(new_deprecated_section);
            }
        }
        if !ConflictOptions.lock().is_empty() || !DeprecatedOptions.lock().is_empty() {
            // Give a warning that the 'instance' section should be used.
            err = Some(LoadError::InstanceSection(ErrConfigInstanceSection {
                conf_file: conf_file.to_owned(),
            }));
        }
        match err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Valid checks if this config is valid.
    // go: Config.Valid L1
    pub fn valid(&mut self) -> Result<(), Error> {
        let fmt_nb = |b: NullableBool| format!("{{{} {}}}", b.is_valid, b.is_true);
        if self.log.enable_error_stack == self.log.disable_error_stack
            && self.log.enable_error_stack != nbUnset
        {
            log::warn!(
                "\"enable-error-stack\" ({}) conflicts \"disable-error-stack\" ({}). \"disable-error-stack\" is deprecated, please use \"enable-error-stack\" instead. disable-error-stack is ignored.",
                fmt_nb(self.log.enable_error_stack),
                fmt_nb(self.log.disable_error_stack)
            );
            // if two options conflict, we will use the value of EnableErrorStack
            self.log.disable_error_stack = nbUnset;
        }
        if self.log.enable_timestamp == self.log.disable_timestamp
            && self.log.enable_timestamp != nbUnset
        {
            log::warn!(
                "\"enable-timestamp\" ({}) conflicts \"disable-timestamp\" ({}). \"disable-timestamp\" is deprecated, please use \"enable-timestamp\" instead",
                fmt_nb(self.log.enable_timestamp),
                fmt_nb(self.log.disable_timestamp)
            );
            // if two options conflict, we will use the value of EnableTimestamp
            self.log.disable_timestamp = nbUnset;
        }
        if self.security.skip_grant_table && !has_root_privilege() {
            return Err(errors::new(
                "TiDB run with skip-grant-table need root privilege",
            ));
        }
        if !ValidStorage
            .get(self.store.as_str())
            .copied()
            .unwrap_or(false)
        {
            let name_list: Vec<&str> = ValidStorage
                .iter()
                .filter(|(_, v)| **v)
                .map(|(k, _)| *k)
                .collect();
            return Err(errors::errorf(
                "invalid store=%s, valid storages=%v",
                &[
                    Arg::from(self.store.clone()),
                    Arg::from(format!("[{}]", name_list.join(" "))),
                ],
            ));
        }
        if self.store == "mocktikv" && !self.instance.tidb_enable_ddl.load() {
            return Err(errors::new("can't disable DDL on mocktikv"));
        }
        if self.max_index_length < DefMaxIndexLength
            || self.max_index_length > DefMaxOfMaxIndexLength
        {
            return Err(errors::errorf(
                "max-index-length should be [%d, %d]",
                &[
                    Arg::from(DefMaxIndexLength),
                    Arg::from(DefMaxOfMaxIndexLength),
                ],
            ));
        }
        if self.index_limit < DefIndexLimit || self.index_limit > DefMaxOfIndexLimit {
            return Err(errors::errorf(
                "index-limit should be [%d, %d]",
                &[Arg::from(DefIndexLimit), Arg::from(DefMaxOfIndexLimit)],
            ));
        }
        if self.log.file.file_log_config.max_size > MaxLogFileSize {
            return Err(errors::errorf(
                "invalid max log file size=%v which is larger than max=%v",
                &[
                    Arg::from(self.log.file.file_log_config.max_size),
                    Arg::from(MaxLogFileSize),
                ],
            ));
        }
        if self.table_column_count_limit < DefTableColumnCountLimit
            || self.table_column_count_limit > DefMaxOfTableColumnCountLimit
        {
            return Err(errors::errorf(
                "table-column-limit should be [%d, %d]",
                &[
                    Arg::from(DefIndexLimit),
                    Arg::from(DefMaxOfTableColumnCountLimit),
                ],
            ));
        }
        if self.instance.plugin_audit_log_buffer_size < 0
            || self.instance.plugin_audit_log_buffer_size > MaxPluginAuditLogBufferSize
        {
            return Err(errors::errorf(
                "plugin-audit-log-buffer-size should be [%d, %d]",
                &[Arg::from(0isize), Arg::from(MaxPluginAuditLogBufferSize)],
            ));
        }
        if self.instance.plugin_audit_log_flush_interval <= 0
            || self.instance.plugin_audit_log_flush_interval > MaxPluginAuditLogFlushInterval
        {
            return Err(errors::errorf(
                "plugin-audit-log-flush-interval should be [%d, %d]",
                &[Arg::from(1isize), Arg::from(MaxPluginAuditLogFlushInterval)],
            ));
        }
        // txn-local-latches
        self.txn_local_latches.valid()?;

        // For tikvclient.
        self.tikv_client.valid()?;
        self.trx_summary.valid()?;

        if self.performance.txn_total_size_limit > 1 << 40 {
            return Err(errors::errorf(
                "txn-total-size-limit should be less than %d",
                &[Arg::from(1isize << 40)],
            ));
        }

        if self.instance.memory_usage_alarm_ratio > 1.0
            || self.instance.memory_usage_alarm_ratio < 0.0
        {
            return Err(errors::new(
                "tidb_memory_usage_alarm_ratio in [Instance] must be greater than or equal to 0 and less than or equal to 1",
            ));
        }

        let engines = self.isolation_read.engines.as_deref().unwrap_or_default();
        if engines.is_empty() {
            return Err(errors::new(
                "the number of [isolation-read]engines for isolation read should be at least 1",
            ));
        }
        for engine in engines {
            if engine != "tidb" && engine != "tikv" && engine != "tiflash" {
                return Err(errors::errorf(
                    "type of [isolation-read]engines can't be %v should be one of tidb or tikv or tiflash",
                    &[Arg::from(engine.clone())],
                ));
            }
        }

        // test security
        self.security.spilled_file_encryption_method =
            crate::gostd::to_lower(&self.security.spilled_file_encryption_method);
        match self.security.spilled_file_encryption_method.as_str() {
            SpilledFileEncryptionMethodPlaintext | SpilledFileEncryptionMethodAES128CTR => {}
            _ => {
                return Err(errors::errorf(
                    "unsupported [security]spilled-file-encryption-method %v, TiDB only supports [%v, %v]",
                    &[
                        Arg::from(self.security.spilled_file_encryption_method.clone()),
                        Arg::from(SpilledFileEncryptionMethodPlaintext),
                        Arg::from(SpilledFileEncryptionMethodAES128CTR),
                    ],
                ));
            }
        }

        // check stats load config
        if self.performance.stats_load_concurrency < DefStatsLoadConcurrencyLimit
            || self.performance.stats_load_concurrency > DefMaxOfStatsLoadConcurrencyLimit
        {
            return Err(errors::errorf(
                "stats-load-concurrency should be [%d, %d]",
                &[
                    Arg::from(DefStatsLoadConcurrencyLimit),
                    Arg::from(DefMaxOfStatsLoadConcurrencyLimit),
                ],
            ));
        }
        if self.performance.stats_load_queue_size < DefStatsLoadQueueSizeLimit
            || self.performance.stats_load_queue_size > DefMaxOfStatsLoadQueueSizeLimit
        {
            return Err(errors::errorf(
                "stats-load-queue-size should be [%d, %d]",
                &[
                    Arg::from(DefStatsLoadQueueSizeLimit as isize),
                    Arg::from(DefMaxOfStatsLoadQueueSizeLimit as isize),
                ],
            ));
        }

        // Check tiflash_compute topo fetch is valid.
        if self.disaggregated_tiflash && self.use_auto_scaler {
            if !tiflashcompute_is_valid_auto_scaler_config(&self.tiflash_compute_auto_scaler_type) {
                return Err(errors::errorf(
                    "invalid AutoScaler type, expect %s, %s or %s, got %s",
                    &[
                        Arg::from("mock"),
                        Arg::from("aws"),
                        Arg::from("gcp"),
                        Arg::from(self.tiflash_compute_auto_scaler_type.clone()),
                    ],
                ));
            }
            if self.tiflash_compute_auto_scaler_addr.is_empty() {
                return Err(errors::new(
                    "autoscaler-addr cannot be empty when disaggregated-tiflash mode is true",
                ));
            }
        }

        // test log level
        zap_level_unmarshal_text(self.log.level.as_bytes())
    }
}

/// `tiflashcompute.IsValidAutoScalerConfig` (block DEF.MPP; a pure function copied here so
/// that tidb-base keeps no upward dependency).
// glue: boundary copy of pkg/util/tiflashcompute.IsValidAutoScalerConfig (block DEF.MPP)
fn tiflashcompute_is_valid_auto_scaler_config(typ: &str) -> bool {
    matches!(typ, "mock" | "aws" | "gcp")
}

/// `zap.NewAtomicLevel().UnmarshalText(text)`: the level must be one zap knows.
// go-dep: go.uber.org/zap@v1.27.1.Level.UnmarshalText L1
fn zap_level_unmarshal_text(text: &[u8]) -> Result<(), Error> {
    let known = |t: &[u8]| {
        matches!(
            t,
            b"debug"
                | b"info"
                | b""
                | b"warn"
                | b"warning"
                | b"error"
                | b"dpanic"
                | b"panic"
                | b"fatal"
        )
    };
    let lower = crate::gostd::to_lower(&String::from_utf8_lossy(text));
    if !known(text) && !known(lower.as_bytes()) {
        return Err(errors::errorf(
            "unrecognized level: %q",
            &[Arg::Bytes(text.to_vec())],
        ));
    }
    Ok(())
}

impl TikvTxnLocalLatches {
    /// Valid returns true if the configuration is valid.
    // go-dep: github.com/tikv/client-go/v2@v2.0.8-0.20260803075849-c3b50791b9fb.TxnLocalLatches.Valid L1
    pub fn valid(&self) -> Result<(), Error> {
        if self.enabled && self.capacity == 0 {
            return Err(errors::new("txn-local-latches.capacity can not be 0"));
        }
        Ok(())
    }
}

impl TikvTiKVClient {
    /// Valid checks if this config is valid.
    // go-dep: github.com/tikv/client-go/v2@v2.0.8-0.20260803075849-c3b50791b9fb.TiKVClient.Valid L1 [M-F2I]
    pub fn valid(&self) -> Result<(), Error> {
        if self.grpc_connection_count == 0 {
            return Err(errors::new(
                "grpc-connection-count should be greater than 0",
            ));
        }
        if self.grpc_compression_type != "none" && self.grpc_compression_type != "gzip" {
            return Err(errors::errorf(
                "grpc-compression-type should be none or %s, but got %s",
                &[
                    Arg::from("gzip"),
                    Arg::from(self.grpc_compression_type.clone()),
                ],
            ));
        }
        // GetGrpcKeepAliveTimeout: time.Duration(GrpcKeepAliveTimeout * float64(time.Second))
        let timeout = crate::gostd::conv::f64_to_i64(self.grpc_keep_alive_timeout * 1e9); // [M-F2I]
        if timeout < 50_000_000 {
            return Err(errors::errorf(
                "grpc-keepalive-timeout should be at least 0.05, but got %f",
                &[Arg::from(self.grpc_keep_alive_timeout)],
            ));
        }
        Ok(())
    }
}

/// UpdateGlobal updates the global config, and provide a restore function that can be used
/// to restore to the original.
// go: UpdateGlobal L1
pub fn update_global(f: impl FnOnce(&mut Config)) {
    update_global_config(f);
}

/// Serializes the read-modify-write updates of the global config ([`update_global_config`]).
/// Go mutates fields of the shared `*Config` in place, so writers of different fields never
/// lose each other's update; the Rust config is an immutable snapshot (M-SNAP), so a clone →
/// modify → store must not interleave with another one.
// glue: writer lock of the M-SNAP global config
static ConfigWriteLock: Mutex<()> = Mutex::new(());

/// Clones the global config, applies `f` and stores the copy, with concurrent writers
/// serialized by [`ConfigWriteLock`] (no lost update; readers see either snapshot).
// glue: in-place write of fields of the global *Config (M-SNAP)
pub fn update_global_config(f: impl FnOnce(&mut Config)) {
    let _guard = ConfigWriteLock.lock();
    let mut new_conf = Config::clone(&get_global_config());
    f(&mut new_conf);
    store_global_config(Arc::new(new_conf));
}

/// RestoreFunc gets a function that restore the config to the current value.
// go: RestoreFunc L1
pub fn restore_func() -> impl FnOnce() {
    let g = get_global_config_arc();
    move || store_global_config(g)
}

// go: hasRootPrivilege L1
fn has_root_privilege() -> bool {
    rustix::process::geteuid().is_root()
}

/// TableLockEnabled uses to check whether enabled the table lock feature.
// go: TableLockEnabled L1
pub fn table_lock_enabled() -> bool {
    get_global_config().enable_table_lock
}

/// TableLockDelayClean uses to get the time of delay clean table lock. (Go: a package var
/// holding a func, replaceable in tests.)
// glue: the func value of the package var TableLockDelayClean
pub fn table_lock_delay_clean() -> u64 {
    get_global_config().delay_clean_table_lock
}

/// `logutil.LogConfig` (pkg/util/logutil: `log.Config` of pingcap/log plus the slow query and
/// general log files): the fields `ToLogConfig` sets. Boundary shape: the logger itself is
/// built by the logging block.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LogConfig {
    pub level: String,
    pub format: String,
    pub disable_timestamp: bool,
    pub file: ZaplogFileLogConfig,
    pub disable_error_verbose: bool,
    pub timeout: isize,
    /// SlowQueryFile filename, default to File log config on empty.
    pub slow_query_file: String,
    /// GeneralLogFile filenanme, default to File log config on empty.
    pub general_log_file: String,
}

impl Log {
    /// ToLogConfig converts *Log to *logutil.LogConfig.
    // go: Log.ToLogConfig L1
    pub fn to_log_config(&self) -> LogConfig {
        // logutil.NewLogConfig with the two options.
        LogConfig {
            level: self.level.clone(),
            format: self.format.clone(),
            disable_timestamp: self.get_disable_timestamp(),
            file: self.file.file_log_config.clone(),
            disable_error_verbose: self.get_disable_error_stack(),
            timeout: self.timeout,
            slow_query_file: self.slow_query_file.clone(),
            general_log_file: self.general_log_file.clone(),
        }
    }
}

/// `jaeger-client-go/config.Configuration`: the fields `ToTracingConfig` sets. Boundary shape:
/// tracing is not ported.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TracingConfiguration {
    pub disabled: bool,
    pub rpc_metrics: bool,
    pub reporter: TracingReporterConfig,
    pub sampler: TracingSamplerConfig,
}

/// `jaeger-client-go/config.ReporterConfig` (the fields `ToTracingConfig` sets).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TracingReporterConfig {
    pub queue_size: isize,
    pub buffer_flush_interval: Duration,
    pub log_spans: bool,
    pub local_agent_host_port: String,
}

/// `jaeger-client-go/config.SamplerConfig` (the fields `ToTracingConfig` sets).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TracingSamplerConfig {
    pub type_: String,
    pub param: f64,
    pub sampling_server_url: String,
    pub max_operations: isize,
    pub sampling_refresh_interval: Duration,
}

impl OpenTracing {
    /// ToTracingConfig converts *OpenTracing to *tracing.Configuration.
    // go: OpenTracing.ToTracingConfig L1
    pub fn to_tracing_config(&self) -> TracingConfiguration {
        let mut ret = TracingConfiguration {
            disabled: !self.enable,
            rpc_metrics: self.rpc_metrics,
            ..Default::default()
        };
        ret.reporter.queue_size = self.reporter.queue_size;
        ret.reporter.buffer_flush_interval = self.reporter.buffer_flush_interval;
        ret.reporter.log_spans = self.reporter.log_spans;
        ret.reporter.local_agent_host_port = self.reporter.local_agent_host_port.clone();

        ret.sampler.type_ = self.sampler.type_.clone();
        ret.sampler.param = self.sampler.param;
        ret.sampler.sampling_server_url = self.sampler.sampling_server_url.clone();
        ret.sampler.max_operations = self.sampler.max_operations;
        ret.sampler.sampling_refresh_interval = self.sampler.sampling_refresh_interval;
        ret
    }
}

// go: init L1
fn init(g: &Globals) {
    init_by_ld_flags(
        g,
        crate::config::versioninfo_TiDBEdition,
        checkBeforeDropLDFlag,
    );
}

/// `versioninfo.TiDBEdition` (pkg/util/versioninfo, set by the linker in Go; Community by
/// default).
pub const versioninfo_TiDBEdition: &str = "Community";

// go: initByLDFlags L1
fn init_by_ld_flags(g: &Globals, _edition: &str, check_before_drop_ld_flag: &str) {
    let conf = defaults_gen::default_conf();
    store_global_config_in(g, Arc::new(conf));
    if check_before_drop_ld_flag == "1" {
        CheckTableBeforeDrop.store(true, Ordering::SeqCst);
    }
}

/// hideConfig is used to filter a single line of config for hiding.
static hideConfig: &[&str] = &["performance.index-usage-sync-lease"];

/// GetJSONConfig returns the config as JSON with hidden items removed It replaces the
/// earlier HideConfig() which used strings.Split() in an way that didn't work for similarly
/// named items (like enable).
// go: GetJSONConfig L1
pub fn get_json_config() -> Result<String, Error> {
    json_config_of(&get_global_config())
}

/// [`get_json_config`] of a given config.
// glue: GetJSONConfig with the global config passed in (vectors)
pub fn json_config_of(conf: &Config) -> Result<String, Error> {
    // json.Marshal + json.Unmarshal into map[string]any: numbers become float64.
    let mut json_value = match &mut reflect::to_json(conf, true)? {
        json::Value::Object(m) => std::mem::take(m),
        _ => unreachable!("a struct encodes as an object"),
    };

    let removed_paths = removedConfig.iter().chain(hideConfig.iter());
    for path in removed_paths {
        let s: Vec<&str> = path.split('.').collect();
        let mut cur_value = &mut json_value;
        for (i, key) in s.iter().enumerate() {
            if i == s.len() - 1 {
                cur_value.remove(*key);
            }
            match cur_value.get_mut(*key) {
                None | Some(json::Value::Null) => break,
                Some(json::Value::Object(m)) => cur_value = m,
                Some(_) => break,
            }
        }
    }

    Ok(json::marshal_indent(&json::Value::Object(json_value), "\t"))
}

/// ContainHiddenConfig checks whether it contains the configuration that needs to be hidden.
// go: ContainHiddenConfig L1
pub fn contain_hidden_config(s: &str) -> bool {
    let s = crate::gostd::to_lower(s);
    if hideConfig.iter().any(|hc| s.contains(hc)) {
        return true;
    }
    removedConfig.iter().any(|dc| s.contains(dc))
}

/// GetGlobalKeyspaceName is used to get global keyspace name from config file or command
/// line.
// go: GetGlobalKeyspaceName L1
pub fn get_global_keyspace_name() -> String {
    get_global_config().keyspace_name.clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conf_file(name: &str, data: &str) -> String {
        let dir =
            std::env::temp_dir().join(format!("tidb-base-config-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("config.toml");
        std::fs::write(&p, data).unwrap();
        p.to_string_lossy().into_owned()
    }

    #[test]
    fn is_all_removed() {
        assert!(is_all_removed_config_items(&[]));
        assert!(is_all_removed_config_items(&[
            "oom-action".to_owned(),
            "log.slow-threshold".to_owned()
        ]));
        assert!(!is_all_removed_config_items(&[
            "oom-action".to_owned(),
            "unknown".to_owned()
        ]));
    }

    /// `InitializeConfig` (exempt from the vectors: it exits the process in Go).
    #[test]
    fn initialize_config_paths() {
        let restore = restore_func();
        // configCheck without a config file: exit 1.
        assert_eq!(
            initialize_config("", true, false, |_| {}),
            Err(InitializeExit::Exit(1))
        );
        // configCheck with a valid file: exit 0, the global config is not stored.
        let ok = conf_file("ok", "port = 4100\n");
        assert_eq!(
            initialize_config(&ok, true, false, |_| {}),
            Err(InitializeExit::Exit(0))
        );
        assert_eq!(get_global_config().port, DefPort);
        // An unknown key is a warning unless configCheck / configStrict.
        let unknown = conf_file("unknown", "port = 4101\nunknown-key = 1\n");
        assert_eq!(
            initialize_config(&unknown, false, false, |c| c.host = "h".to_owned()),
            Ok(())
        );
        assert_eq!(
            (get_global_config().port, get_global_config().host.as_str()),
            (4101, "h")
        );
        let strict = std::panic::catch_unwind(|| initialize_config(&unknown, false, true, |_| {}));
        assert!(
            strict.is_err(),
            "terror.MustNil panics on the validation error"
        );
        // Only removed items: a warning even when strict.
        let removed = conf_file("removed", "port = 4102\noom-action = \"log\"\n");
        assert_eq!(initialize_config(&removed, false, true, |_| {}), Ok(()));
        assert_eq!(get_global_config().port, 4102);
        // Invalid config: exit 1.
        let invalid = conf_file("invalid", "index-limit = 1\n");
        assert_eq!(
            initialize_config(&invalid, false, false, |_| {}),
            Err(InitializeExit::Exit(1))
        );
        restore();
        assert_eq!(get_global_config().port, DefPort);
    }
}
