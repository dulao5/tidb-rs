//! go-file: pkg/sessionctx/variable/tidb_vars.go (the defaults and process-wide state D05.1 reads)
//!
//! The `Def*` constants and the process-wide atomics (`atomic.New*` package variables) the
//! validation hooks of block D05.1 read or write. The SetGlobal hooks (block D05.2) write the
//! rest of the 88 atomics; they are added with those hooks.

#![allow(non_upper_case_globals)]

use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, AtomicU32, AtomicU64, Ordering::SeqCst};

use super::tidb_defs::*;

/// Default TiDB system variable values.
pub const DefTiDBEnableNoopFuncs: &str = super::variable::Off;
/// See [`DefTiDBEnableNoopFuncs`].
pub const DefTiDBAllowMPPExecution: bool = true;
/// See [`DefTiDBEnableNoopFuncs`].
pub const DefTiDBForeignKeyChecks: bool = true;
/// See [`DefTiDBEnableNoopFuncs`].
pub const DefTiDBRowFormatV2: i64 = 2;
/// See [`DefTiDBEnableNoopFuncs`].
pub const DefTiDBDDLReorgWorkerCount: i32 = 4;
/// See [`DefTiDBEnableNoopFuncs`].
pub const DefTiDBDDLReorgBatchSize: i32 = 256;
/// See [`DefTiDBEnableNoopFuncs`].
pub const DefTiDBDDLFlashbackConcurrency: i32 = 64;
/// See [`DefTiDBEnableNoopFuncs`].
pub const DefTiDBDDLErrorCountLimit: i64 = 512;
/// See [`DefTiDBEnableNoopFuncs`].
pub const DefTiDBMaxDeltaSchemaCount: i64 = 1024;
/// See [`DefTiDBEnableNoopFuncs`].
pub const DefTimestampFloat: f64 = 0.0;
/// See [`DefTiDBEnableNoopFuncs`].
pub const DefTiDBMemQuotaQuery: i64 = 1 << 30; // memory.DefMemQuotaQuery (1GB)
/// See [`DefTiDBEnableNoopFuncs`].
pub const DefTiDBEnableAutoAnalyze: bool = true;
/// See [`DefTiDBEnableNoopFuncs`].
pub const DefTiDBEnableAutoAnalyzePriorityQueue: bool = true;
/// See [`DefTiDBEnableNoopFuncs`].
pub const DefTiDBMaxDistTaskNodes: isize = -1;
/// See [`DefTiDBEnableNoopFuncs`].
pub const DefTiDBMPPStoreFailTTL: &str = "0s";
/// DefTiDBGOGCTunerThreshold is to limit TiDBGOGCTunerThreshold.
pub const DefTiDBGOGCTunerThreshold: f64 = 0.6;
/// See [`DefTiDBEnableNoopFuncs`].
pub const DefTiDBGOGCMaxValue: i64 = 500;
/// See [`DefTiDBEnableNoopFuncs`].
pub const DefTiDBGOGCMinValue: i64 = 100;
/// See [`DefTiDBEnableNoopFuncs`].
pub const DefRuntimeFilterType: &str = "IN";
/// See [`DefTiDBEnableNoopFuncs`].
pub const DefRuntimeFilterMode: &str = "OFF";

/// RunAutoAnalyze indicates whether auto analyze should run.
pub static RunAutoAnalyze: AtomicBool = AtomicBool::new(DefTiDBEnableAutoAnalyze);
/// EnableAutoAnalyzePriorityQueue indicates whether to enable the priority queue for auto analyze.
pub static EnableAutoAnalyzePriorityQueue: AtomicBool =
    AtomicBool::new(DefTiDBEnableAutoAnalyzePriorityQueue);
/// GOGCTunerThreshold is the threshold for GOGC tuner (an `atomic.Float64`: the f64 bits).
pub static GOGCTunerThreshold: AtomicF64 = AtomicF64::new(DefTiDBGOGCTunerThreshold);
/// PasswordValidationLength is the minimum number of characters a password must have.
pub static PasswordValidationLength: AtomicI32 = AtomicI32::new(8);
/// PasswordValidationMixedCaseCount is the minimum number of lowercase and uppercase characters a password must have.
pub static PasswordValidationMixedCaseCount: AtomicI32 = AtomicI32::new(1);
/// PasswordValidtaionNumberCount is the minimum number of numeric characters a password must have.
pub static PasswordValidtaionNumberCount: AtomicI32 = AtomicI32::new(1);
/// PasswordValidationSpecialCharCount is the minimum number of special characters a password must have.
pub static PasswordValidationSpecialCharCount: AtomicI32 = AtomicI32::new(1);
/// ConnectAttrsLongestSeen tracks the largest connection attribute aggregate size seen so far.
pub static ConnectAttrsLongestSeen: AtomicI64 = AtomicI64::new(0);
/// ConnectAttrsLost counts the number of connections whose attributes were truncated.
pub static ConnectAttrsLost: AtomicI64 = AtomicI64::new(0);

/// `go.uber.org/atomic.Float64`.
// glue: atomic.Float64 (f64 bits in an AtomicU64)
pub struct AtomicF64(AtomicU64);

impl AtomicF64 {
    /// A new value.
    // glue: atomic.NewFloat64
    pub const fn new(v: f64) -> AtomicF64 {
        AtomicF64(AtomicU64::new(v.to_bits()))
    }

    /// Load.
    // glue: atomic.Float64.Load
    pub fn load(&self) -> f64 {
        f64::from_bits(self.0.load(SeqCst))
    }

    /// Store.
    // glue: atomic.Float64.Store
    pub fn store(&self, v: f64) {
        self.0.store(v.to_bits(), SeqCst);
    }
}

/// `gctuner.MinGCPercent()` / `MaxGCPercent()`: the bounds of the Go GC tuner. tidb-rs has no
/// Go GC (runtime-only variables, N8 §3.4); the bounds are kept so that the validation of
/// tidb_gogc_tuner_max_value / _min_value behaves as in Go.
// go-dep: github.com/pingcap/tidb/pkg/util/gctuner.minGCPercent L2 [runtime-only]
pub static minGCPercent: AtomicU32 = AtomicU32::new(100);
/// See [`minGCPercent`].
// go-dep: github.com/pingcap/tidb/pkg/util/gctuner.maxGCPercent L2 [runtime-only]
pub static maxGCPercent: AtomicU32 = AtomicU32::new(500);
/// `gctuner.GlobalMemoryLimitTuner.GetPercentage()`: 0 until tidb_server_memory_limit_gc_trigger
/// is set globally (its SetGlobal hook, block D05.2).
// go-dep: github.com/pingcap/tidb/pkg/util/gctuner.memoryLimitTuner.percentage L2 [runtime-only]
pub static memoryLimitTunerPercentage: AtomicF64 = AtomicF64::new(0.0);

/// `memory.GetMemTotalIgnoreErr()`.
// glue: call into tidb-memory (D15.3)
pub(crate) fn mem_total_ignore_err() -> u64 {
    tidb_memory::meminfo::get_mem_total_ignore_err()
}

// go: serverMemoryLimitDefaultValue L1
pub(crate) fn server_memory_limit_default_value() -> &'static str {
    match tidb_memory::meminfo::mem_total() {
        Ok(total) if total != 0 => "80%",
        _ => "0",
    }
}

/// `mustParseDuration`: a duration literal of the defaults; panics on a bad literal.
// go: mustParseDuration L1
pub(crate) fn must_parse_duration(s: &str) -> i64 {
    match tidb_base::gostd::time::parse_duration(s) {
        Ok(d) => d,
        Err(_) => panic!("{s} is not a duration"),
    }
}

/// Test access to the unexported default helpers (govec op `defaults`).
// glue: test entry point
#[doc(hidden)]
pub fn defaults_for_test(dur: &str) -> (&'static str, i64) {
    (
        server_memory_limit_default_value(),
        must_parse_duration(dur),
    )
}

/// ConcurrencyUnset means the value the of the concurrency related variable is unset.
pub const ConcurrencyUnset: i64 = -1;
/// Go: `DefMinPagingSize = int(paging.MinPagingSize)` (pkg/util/paging: 128).
pub const DefMinPagingSize: i64 = 128;
/// Go: `DefMaxPagingSize = int(paging.MaxPagingSize)` (pkg/util/paging: 50000).
pub const DefMaxPagingSize: i64 = 50000;
/// Go: `DefTiDBOptRangeMaxSize = 64 * int64(size.MB)` (64 MB).
pub const DefTiDBOptRangeMaxSize: i64 = 64 << 20;
/// Go: `logutil.DefaultSlowTxnThreshold` (pkg/util/logutil: 0).
pub const DefaultSlowTxnThreshold: u64 = 0;

/// Go: `DefTiDBForcePriority` (tidb_vars.go).
pub const DefTiDBForcePriority: tidb_base::mysql::PriorityEnum = tidb_base::mysql::NoPriority;
/// Go: `DefTiDBAutoAnalyzePartitionBatchSize` (tidb_vars.go).
pub const DefTiDBAutoAnalyzePartitionBatchSize: i64 = tidb_base::mysql::PartitionCountLimit as i64;
/// Go: `DefTiDBInstancePlanCacheMaxMemSize` (tidb_vars.go).
pub const DefTiDBInstancePlanCacheMaxMemSize: i64 = 100 << 20;

// ---------------------------------------------------------------- process-wide state of the SetGlobal / GetGlobal hooks (D05.2 T5)

/// `go.uber.org/atomic.String`: a string read and replaced atomically.
// glue: atomic.String (a lazily initialised RwLock<String>)
pub struct GoAtomicString {
    init: fn() -> String,
    cell: std::sync::OnceLock<parking_lot::RwLock<String>>,
}

impl GoAtomicString {
    /// A string initialised on first use with `init` (Go: `atomic.NewString(init)` at package init).
    // glue: atomic.NewString
    pub const fn new_lazy(init: fn() -> String) -> Self {
        GoAtomicString {
            init,
            cell: std::sync::OnceLock::new(),
        }
    }

    // glue: the lazily created cell
    fn cell(&self) -> &parking_lot::RwLock<String> {
        self.cell
            .get_or_init(|| parking_lot::RwLock::new((self.init)()))
    }

    /// Load atomically loads the wrapped string.
    // glue: atomic.String.Load
    pub fn load(&self) -> String {
        self.cell().read().clone()
    }

    /// Store atomically stores the passed string.
    // glue: atomic.String.Store
    pub fn store(&self, v: String) {
        *self.cell().write() = v;
    }
}

/// Process global variables.
pub static ProcessGeneralLog: AtomicBool = AtomicBool::new(false);
/// AnalyzeColumnOptions is a global variable that indicates the default column choice for ANALYZE.
/// The value of this variable is a string that can be one of the following values:
/// "PREDICATE", "ALL".
/// The behavior of the analyze operation depends on the value of `tidb_persist_analyze_options`:
/// 1. If `tidb_persist_analyze_options` is enabled and the column choice from the analyze options record is set to `default`,
///    the value of `tidb_analyze_column_options` determines the behavior of the analyze operation.
/// 2. If `tidb_persist_analyze_options` is disabled, `tidb_analyze_column_options` is used directly to decide
///    whether to analyze all columns or just the predicate columns.
pub static AnalyzeColumnOptions: GoAtomicString =
    GoAtomicString::new_lazy(|| DefTiDBAnalyzeColumnOptions.to_owned());
/// GlobalLogMaxDays.
pub static GlobalLogMaxDays: LazyLock<AtomicI32> = LazyLock::new(|| {
    AtomicI32::new(
        tidb_base::config::get_global_config()
            .log
            .file
            .file_log_config
            .max_days as i32,
    )
});
/// QueryLogMaxLen.
pub static QueryLogMaxLen: AtomicI32 = AtomicI32::new(DefTiDBQueryLogMaxLen as _);
/// EnablePProfSQLCPU.
pub static EnablePProfSQLCPU: AtomicBool = AtomicBool::new(false);
/// EnableBatchDML.
pub static EnableBatchDML: AtomicBool = AtomicBool::new(false);
/// EnableTmpStorageOnOOM.
pub static EnableTmpStorageOnOOM: AtomicBool = AtomicBool::new(DefTiDBEnableTmpStorageOnOOM);
/// ddlReorgWorkerCounter.
/// (read and written with sync/atomic in Go)
pub static ddlReorgWorkerCounter: AtomicI32 = AtomicI32::new(DefTiDBDDLReorgWorkerCount as _);
/// ddlReorgBatchSize.
/// (read and written with sync/atomic in Go)
pub static ddlReorgBatchSize: AtomicI32 = AtomicI32::new(DefTiDBDDLReorgBatchSize as _);
/// ddlFlashbackConcurrency.
/// (read and written with sync/atomic in Go)
pub static ddlFlashbackConcurrency: AtomicI32 = AtomicI32::new(DefTiDBDDLFlashbackConcurrency as _);
/// ddlErrorCountLimit.
/// (read and written with sync/atomic in Go)
pub static ddlErrorCountLimit: AtomicI64 = AtomicI64::new(DefTiDBDDLErrorCountLimit as _);
/// ddlReorgRowFormat.
/// (read and written with sync/atomic in Go)
pub static ddlReorgRowFormat: AtomicI64 = AtomicI64::new(DefTiDBRowFormatV2 as _);
/// DDLReorgMaxWriteSpeed.
pub static DDLReorgMaxWriteSpeed: AtomicI64 = AtomicI64::new(DefTiDBDDLReorgMaxWriteSpeed as _);
/// maxDeltaSchemaCount.
/// (read and written with sync/atomic in Go)
pub static maxDeltaSchemaCount: AtomicI64 = AtomicI64::new(DefTiDBMaxDeltaSchemaCount as _);
/// DDLSlowOprThreshold is the threshold for ddl slow operations, uint is millisecond.
/// (a `uint32` read and written with sync/atomic)
pub static DDLSlowOprThreshold: LazyLock<AtomicU32> = LazyLock::new(|| {
    AtomicU32::new(
        tidb_base::config::get_global_config()
            .instance
            .ddl_slow_opr_threshold,
    )
});
/// ForcePriority.
/// (an `int32` read and written with sync/atomic)
pub static ForcePriority: AtomicI32 = AtomicI32::new(DefTiDBForcePriority.0 as i32);
/// MaxOfMaxAllowedPacket.
/// (read and written with sync/atomic in Go)
pub static MaxOfMaxAllowedPacket: AtomicU64 = AtomicU64::new(1073741824 as _);
/// ExpensiveQueryTimeThreshold.
/// (read and written with sync/atomic in Go)
pub static ExpensiveQueryTimeThreshold: AtomicU64 =
    AtomicU64::new(DefTiDBExpensiveQueryTimeThreshold as _);
/// ExpensiveTxnTimeThreshold.
/// (read and written with sync/atomic in Go)
pub static ExpensiveTxnTimeThreshold: AtomicU64 =
    AtomicU64::new(DefTiDBExpensiveTxnTimeThreshold as _);
/// MemoryUsageAlarmRatio.
pub static MemoryUsageAlarmRatio: AtomicF64 = AtomicF64::new(DefMemoryUsageAlarmRatio);
/// MemoryUsageAlarmKeepRecordNum.
pub static MemoryUsageAlarmKeepRecordNum: AtomicI64 =
    AtomicI64::new(DefMemoryUsageAlarmKeepRecordNum as _);
/// EnableLocalTxn.
pub static EnableLocalTxn: AtomicBool = AtomicBool::new(DefTiDBEnableLocalTxn);
/// MaxTSOBatchWaitInterval.
pub static MaxTSOBatchWaitInterval: AtomicF64 = AtomicF64::new(DefTiDBTSOClientBatchMaxWaitTime);
/// EnableTSOFollowerProxy.
pub static EnableTSOFollowerProxy: AtomicBool = AtomicBool::new(DefTiDBEnableTSOFollowerProxy);
/// EnablePDFollowerHandleRegion.
pub static EnablePDFollowerHandleRegion: AtomicBool =
    AtomicBool::new(DefPDEnableFollowerHandleRegion);
/// EnableBatchQueryRegion.
pub static EnableBatchQueryRegion: AtomicBool = AtomicBool::new(DefTiDBEnableBatchQueryRegion);
/// RestrictedReadOnly.
pub static RestrictedReadOnly: AtomicBool = AtomicBool::new(DefTiDBRestrictedReadOnly);
/// VarTiDBSuperReadOnly.
pub static VarTiDBSuperReadOnly: AtomicBool = AtomicBool::new(DefTiDBSuperReadOnly);
/// PersistAnalyzeOptions.
pub static PersistAnalyzeOptions: AtomicBool = AtomicBool::new(DefTiDBPersistAnalyzeOptions);
/// TableCacheLease.
pub static TableCacheLease: AtomicI64 = AtomicI64::new(DefTiDBTableCacheLease as _);
/// StatsLoadSyncWait.
pub static StatsLoadSyncWait: AtomicI64 = AtomicI64::new(DefTiDBStatsLoadSyncWait as _);
/// StatsLoadPseudoTimeout.
pub static StatsLoadPseudoTimeout: AtomicBool = AtomicBool::new(DefTiDBStatsLoadPseudoTimeout);
/// MemQuotaBindingCache.
pub static MemQuotaBindingCache: AtomicI64 = AtomicI64::new(DefTiDBMemQuotaBindingCache as _);
/// GCMaxWaitTime.
pub static GCMaxWaitTime: AtomicI64 = AtomicI64::new(DefTiDBGCMaxWaitTime as _);
/// StatsCacheMemQuota.
pub static StatsCacheMemQuota: AtomicI64 = AtomicI64::new(DefTiDBStatsCacheMemQuota as _);
/// OOMAction.
pub static OOMAction: GoAtomicString = GoAtomicString::new_lazy(|| DefTiDBMemOOMAction.to_owned());
/// MaxAutoAnalyzeTime.
pub static MaxAutoAnalyzeTime: AtomicI64 = AtomicI64::new(DefTiDBMaxAutoAnalyzeTime as _);
/// variables for plan cache
pub static PreparedPlanCacheMemoryGuardRatio: AtomicF64 =
    AtomicF64::new(DefTiDBPrepPlanCacheMemoryGuardRatio);
/// EnableInstancePlanCache.
pub static EnableInstancePlanCache: AtomicBool = AtomicBool::new(false);
/// InstancePlanCacheReservedPercentage.
pub static InstancePlanCacheReservedPercentage: AtomicF64 = AtomicF64::new(0.1);
/// InstancePlanCacheMaxMemSize.
pub static InstancePlanCacheMaxMemSize: AtomicI64 =
    AtomicI64::new(DefTiDBInstancePlanCacheMaxMemSize as _);
/// EnableDistTask.
pub static EnableDistTask: AtomicBool = AtomicBool::new(DefTiDBEnableDistTask);
/// EnableFastCreateTable.
pub static EnableFastCreateTable: AtomicBool = AtomicBool::new(DefTiDBEnableFastCreateTable);
/// DDLForce2Queue.
pub static DDLForce2Queue: AtomicBool = AtomicBool::new(false);
/// EnableNoopVariables.
pub static EnableNoopVariables: AtomicBool = AtomicBool::new(DefTiDBEnableNoopVariables);
/// EnableMDL.
pub static EnableMDL: AtomicBool = AtomicBool::new(false);
/// AutoAnalyzePartitionBatchSize.
pub static AutoAnalyzePartitionBatchSize: AtomicI64 =
    AtomicI64::new(DefTiDBAutoAnalyzePartitionBatchSize as _);
/// AutoAnalyzeConcurrency.
pub static AutoAnalyzeConcurrency: AtomicI32 = AtomicI32::new(DefTiDBAutoAnalyzeConcurrency as _);
/// EnableFastReorg indicates whether to use lightning to enhance DDL reorg performance.
pub static EnableFastReorg: AtomicBool = AtomicBool::new(DefTiDBEnableFastReorg);
/// DDLDiskQuota is the temporary variable for set disk quota for lightning
pub static DDLDiskQuota: AtomicU64 = AtomicU64::new(DefTiDBDDLDiskQuota as _);
/// EnableForeignKey indicates whether to enable foreign key feature.
pub static EnableForeignKey: AtomicBool = AtomicBool::new(true);
/// EnableRCReadCheckTS.
pub static EnableRCReadCheckTS: AtomicBool = AtomicBool::new(false);
/// EnableRowLevelChecksum indicates whether to append checksum to row values.
pub static EnableRowLevelChecksum: AtomicBool = AtomicBool::new(DefTiDBEnableRowLevelChecksum);
/// LowResolutionTSOUpdateInterval.
pub static LowResolutionTSOUpdateInterval: AtomicU32 =
    AtomicU32::new(DefTiDBLowResolutionTSOUpdateInterval as _);
/// EnableTTLJob.
pub static EnableTTLJob: AtomicBool = AtomicBool::new(DefTiDBTTLJobEnable);
/// TTLScanBatchSize.
pub static TTLScanBatchSize: AtomicI64 = AtomicI64::new(DefTiDBTTLScanBatchSize as _);
/// TTLDeleteBatchSize.
pub static TTLDeleteBatchSize: AtomicI64 = AtomicI64::new(DefTiDBTTLDeleteBatchSize as _);
/// TTLDeleteRateLimit.
pub static TTLDeleteRateLimit: AtomicI64 = AtomicI64::new(DefTiDBTTLDeleteRateLimit as _);
/// TTLScanWorkerCount.
pub static TTLScanWorkerCount: AtomicI32 = AtomicI32::new(DefTiDBTTLScanWorkerCount as _);
/// TTLDeleteWorkerCount.
pub static TTLDeleteWorkerCount: AtomicI32 = AtomicI32::new(DefTiDBTTLDeleteWorkerCount as _);
/// PasswordHistory.
pub static PasswordHistory: AtomicI64 = AtomicI64::new(DefPasswordReuseHistory as _);
/// PasswordReuseInterval.
pub static PasswordReuseInterval: AtomicI64 = AtomicI64::new(DefPasswordReuseTime as _);
/// IsSandBoxModeEnabled.
pub static IsSandBoxModeEnabled: AtomicBool = AtomicBool::new(false);
/// MaxUserConnectionsValue.
pub static MaxUserConnectionsValue: AtomicU32 = AtomicU32::new(DefMaxUserConnections as _);
/// MaxPreparedStmtCountValue.
pub static MaxPreparedStmtCountValue: AtomicI64 = AtomicI64::new(DefMaxPreparedStmtCount as _);
/// HistoricalStatsDuration.
pub static HistoricalStatsDuration: AtomicI64 = AtomicI64::new(DefTiDBHistoricalStatsDuration); // time.Duration (ns)
/// EnableHistoricalStatsForCapture.
pub static EnableHistoricalStatsForCapture: AtomicBool =
    AtomicBool::new(DefTiDBEnableHistoricalStatsForCapture);
/// TTLRunningTasks.
pub static TTLRunningTasks: AtomicI32 = AtomicI32::new(DefTiDBTTLRunningTasks as _);
/// always set the default value to false because the resource control in kv-client is not inited
/// It will be initialized to the right value after the first call of `rebuildSysVarCache`
pub static EnableResourceControl: AtomicBool = AtomicBool::new(false);
/// EnableResourceControlStrictMode.
pub static EnableResourceControlStrictMode: AtomicBool = AtomicBool::new(true);
/// EnableCheckConstraint.
pub static EnableCheckConstraint: AtomicBool = AtomicBool::new(DefTiDBEnableCheckConstraint);
/// SkipMissingPartitionStats.
pub static SkipMissingPartitionStats: AtomicBool =
    AtomicBool::new(DefTiDBSkipMissingPartitionStats);
/// TiFlashEnablePipelineMode.
pub static TiFlashEnablePipelineMode: AtomicBool =
    AtomicBool::new(DefTiDBEnableTiFlashPipelineMode);
/// ServiceScope.
pub static ServiceScope: GoAtomicString = GoAtomicString::new_lazy(|| "".to_owned());
/// SchemaVersionCacheLimit.
pub static SchemaVersionCacheLimit: AtomicI64 = AtomicI64::new(DefTiDBSchemaVersionCacheLimit as _);
/// CloudStorageURI.
pub static CloudStorageURI: GoAtomicString = GoAtomicString::new_lazy(|| "".to_owned());
/// IgnoreInlistPlanDigest.
pub static IgnoreInlistPlanDigest: AtomicBool = AtomicBool::new(DefTiDBIgnoreInlistPlanDigest);
/// TxnEntrySizeLimit.
pub static TxnEntrySizeLimit: AtomicU64 = AtomicU64::new(DefTiDBTxnEntrySizeLimit as _);
/// SchemaCacheSize.
pub static SchemaCacheSize: AtomicU64 = AtomicU64::new(DefTiDBSchemaCacheSize as _);
/// SchemaCacheSizeOriginText.
pub static SchemaCacheSizeOriginText: GoAtomicString =
    GoAtomicString::new_lazy(|| DefTiDBSchemaCacheSize.to_string());
/// AccelerateUserCreationUpdate.
pub static AccelerateUserCreationUpdate: AtomicBool =
    AtomicBool::new(DefTiDBAccelerateUserCreationUpdate);
/// AdvancerCheckPointLagLimit.
pub static AdvancerCheckPointLagLimit: AtomicI64 =
    AtomicI64::new(DefTiDBAdvancerCheckPointLagLimit); // time.Duration (ns)
/// CircuitBreakerPDMetadataErrorRateThresholdRatio.
pub static CircuitBreakerPDMetadataErrorRateThresholdRatio: AtomicF64 = AtomicF64::new(0.0);
/// ConnectAttrsSize is the max aggregate byte size of connection attributes allowed per connection.
/// Corresponds to performance_schema_session_connect_attrs_size. Default 4096.
pub static ConnectAttrsSize: AtomicI64 = AtomicI64::new(DefConnectAttrsSize as _);

