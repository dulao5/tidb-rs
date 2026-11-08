//! go-file: pkg/sessionctx/variable/tidb_vars.go (the defaults and process-wide state D05.1 reads)
//!
//! The `Def*` constants and the process-wide atomics (`atomic.New*` package variables) the
//! validation hooks of block D05.1 read or write. The SetGlobal hooks (block D05.2) write the
//! rest of the 88 atomics; they are added with those hooks.

#![allow(non_upper_case_globals)]

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, AtomicU32, AtomicU64, Ordering::SeqCst};

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
