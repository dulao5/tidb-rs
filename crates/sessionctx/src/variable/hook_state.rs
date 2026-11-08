//! go-file: pkg/sessionctx/variable/tidb_vars.go
//!
//! Process-wide state that SetGlobal / GetGlobal hooks write outside the variable package
//! (D05.2 PR B2), and the callbacks other packages register into the variable package.
//!
//! - **State cells** (`go-dep`): client-go's `kv.StoreLimit` / `kv.TxnCommitBatchSize`,
//!   `util.CommitterConcurrency`, `util/tls.RequireSecureTransport`, the top-SQL limits,
//!   the gctuner switch, `GenerateBinaryPlan`, the TTL job schedule window and the slow-log
//!   rate limiter. The values are kept (and read back by GetGlobal) like Go; the components
//!   that consume them (client-go, the TLS listener, top SQL, the Go GC tuner, the TTL
//!   worker) are runtime-only or come with later blocks.
//! - **Callbacks** (Go: package-level `func` values / `atomic.Pointer[func]` that the domain,
//!   the DDL owner, the executor or stale read register at start-up): `None` until a
//!   component registers one. Calling an unregistered one panics like Go's nil function
//!   call (`runtime error: invalid memory address or nil pointer dereference`); the hooks
//!   that check for nil first (`SetLowResolutionTSOUpdateInterval`, `ChangeSchemaCacheSize`,
//!   `ChangePDMetadataCircuitBreakerErrorRateThresholdRatio`) do so here too.

#![allow(non_upper_case_globals)]

use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, AtomicU64};

use parking_lot::{Mutex, RwLock};
use tidb_base::Error;

use super::tidb_vars::AtomicF64;
use super::timeparse::{DayTime, must_parse_full_day_time};

/// Go `kv.StoreLimit` (client-go): the per-store request limit (0: unlimited).
// go-dep: github.com/tikv/client-go/v2/kv.StoreLimit L1
pub static StoreLimit: AtomicI64 = AtomicI64::new(0);
/// Go `kv.DefTxnCommitBatchSize` (client-go).
// go-dep: github.com/tikv/client-go/v2/kv.DefTxnCommitBatchSize L1
pub const DefTxnCommitBatchSize: u64 = 16 * 1024;
/// Go `kv.TxnCommitBatchSize` (client-go; stored `DefTxnCommitBatchSize` at init).
// go-dep: github.com/tikv/client-go/v2/kv.TxnCommitBatchSize L1
pub static TxnCommitBatchSize: AtomicU64 = AtomicU64::new(DefTxnCommitBatchSize);
/// Go `util.CommitterConcurrency` (client-go; TiDB stores the sysvar default at start-up).
// go-dep: github.com/tikv/client-go/v2/util.CommitterConcurrency L1
pub static CommitterConcurrency: AtomicI32 =
    AtomicI32::new(super::tidb_defs::DefTiDBCommitterConcurrency as i32);
/// Go `tls.RequireSecureTransport` (pkg/util/tls).
// go-dep: github.com/pingcap/tidb/pkg/util/tls.RequireSecureTransport L1
pub static RequireSecureTransport: AtomicBool = AtomicBool::new(false);
/// Go `topsqlstate.DefTiDBTopSQLMaxTimeSeriesCount`.
// go-dep: github.com/pingcap/tidb/pkg/util/topsql/state.DefTiDBTopSQLMaxTimeSeriesCount L1
pub const DefTiDBTopSQLMaxTimeSeriesCount: i64 = 100;
/// Go `topsqlstate.DefTiDBTopSQLMaxMetaCount`.
// go-dep: github.com/pingcap/tidb/pkg/util/topsql/state.DefTiDBTopSQLMaxMetaCount L1
pub const DefTiDBTopSQLMaxMetaCount: i64 = 5000;
/// Go `topsqlstate.GlobalState.MaxStatementCount`.
// go-dep: github.com/pingcap/tidb/pkg/util/topsql/state.GlobalState L1
pub static TopSQLMaxStatementCount: AtomicI64 = AtomicI64::new(DefTiDBTopSQLMaxTimeSeriesCount);
/// Go `topsqlstate.GlobalState.MaxCollect`.
// go-dep: github.com/pingcap/tidb/pkg/util/topsql/state.GlobalState L1
pub static TopSQLMaxCollect: AtomicI64 = AtomicI64::new(DefTiDBTopSQLMaxMetaCount);
/// Go `gctuner.EnableGOGCTuner` (the Go GC tuner: runtime-only, N8 §3.4).
// go-dep: github.com/pingcap/tidb/pkg/util/gctuner.EnableGOGCTuner L2 [runtime-only]
pub static EnableGOGCTuner: AtomicBool = AtomicBool::new(false);
/// Go `GenerateBinaryPlan` (session.go): binary plans in the slow log and statement summary.
// go-dep: github.com/pingcap/tidb/pkg/sessionctx/variable.GenerateBinaryPlan L1
pub static GenerateBinaryPlan: AtomicBool = AtomicBool::new(false);

/// Go `TTLJobScheduleWindowStartTime` (`mustParseTime(FullDayTimeFormat, "00:00 +0000")`).
// go-dep: github.com/pingcap/tidb/pkg/sessionctx/variable.TTLJobScheduleWindowStartTime L1
pub(crate) static TTLJobScheduleWindowStartTime: LazyLock<Mutex<DayTime>> = LazyLock::new(|| {
    Mutex::new(must_parse_full_day_time(
        super::tidb_defs::DefTiDBTTLJobScheduleWindowStartTime,
    ))
});
/// Go `TTLJobScheduleWindowEndTime` (`mustParseTime(FullDayTimeFormat, "23:59 +0000")`).
// go-dep: github.com/pingcap/tidb/pkg/sessionctx/variable.TTLJobScheduleWindowEndTime L1
pub(crate) static TTLJobScheduleWindowEndTime: LazyLock<Mutex<DayTime>> = LazyLock::new(|| {
    Mutex::new(must_parse_full_day_time(
        super::tidb_defs::DefTiDBTTLJobScheduleWindowEndTime,
    ))
});

/// Go `GlobalSlowLogRateLimiter = rate.NewLimiter(rate.Inf, 1)`: its limit (events per
/// second; `+Inf` = unlimited) and burst. The limiter itself is consumed by the slow log
/// (D14.2).
// go-dep: github.com/pingcap/tidb/pkg/sessionctx/variable.GlobalSlowLogRateLimiter L1
pub static GlobalSlowLogRateLimit: AtomicF64 = AtomicF64::new(f64::INFINITY);
/// The burst of `GlobalSlowLogRateLimiter`.
// go-dep: github.com/pingcap/tidb/pkg/sessionctx/variable.GlobalSlowLogRateLimiter L1
pub static GlobalSlowLogRateBurst: AtomicI64 = AtomicI64::new(1);

/// A registered callback (`None`: Go's nil func).
pub type Callback<F> = RwLock<Option<Arc<F>>>;

/// `func(string, string) error`.
pub type PDClientDynamicOptionFn = dyn Fn(&str, &str) -> Result<(), Error> + Send + Sync;

/// Go's message of a call through a nil func value.
const NIL_FUNC: &str = "runtime error: invalid memory address or nil pointer dereference";

/// The registered callback, or Go's nil-func panic.
// glue: call through a Go package-level func value
pub fn registered<F: ?Sized>(c: &Callback<F>) -> Arc<F> {
    c.read().clone().unwrap_or_else(|| panic!("{NIL_FUNC}"))
}

/// Go `SetMemQuotaAnalyze` (registered by the executor: `GlobalAnalyzeMemoryTracker.SetBytesLimit`).
// go-dep: github.com/pingcap/tidb/pkg/sessionctx/variable.SetMemQuotaAnalyze L1
pub static SetMemQuotaAnalyze: Callback<dyn Fn(i64) + Send + Sync> = RwLock::new(None);
/// Go `GetMemQuotaAnalyze` (registered by the executor: `GlobalAnalyzeMemoryTracker.GetBytesLimit`).
// go-dep: github.com/pingcap/tidb/pkg/sessionctx/variable.GetMemQuotaAnalyze L1
pub static GetMemQuotaAnalyze: Callback<dyn Fn() -> i64 + Send + Sync> = RwLock::new(None);
/// Go `SetStatsCacheCapacity` (registered by the domain).
// go-dep: github.com/pingcap/tidb/pkg/sessionctx/variable.SetStatsCacheCapacity L1
pub static SetStatsCacheCapacity: Callback<dyn Fn(i64) + Send + Sync> = RwLock::new(None);
/// Go `SetPDClientDynamicOption` (registered by the domain).
// go-dep: github.com/pingcap/tidb/pkg/sessionctx/variable.SetPDClientDynamicOption L1
pub static SetPDClientDynamicOption: Callback<PDClientDynamicOptionFn> = RwLock::new(None);
/// Go `SwitchMDL` (registered by the DDL owner).
// go-dep: github.com/pingcap/tidb/pkg/sessionctx/variable.SwitchMDL L1
pub static SwitchMDL: Callback<dyn Fn(bool) -> Result<(), Error> + Send + Sync> = RwLock::new(None);
/// Go `SetExternalTimestamp` (registered by the domain / stale read).
// go-dep: github.com/pingcap/tidb/pkg/sessionctx/variable.SetExternalTimestamp L1
pub static SetExternalTimestamp: Callback<dyn Fn(u64) -> Result<(), Error> + Send + Sync> =
    RwLock::new(None);
/// Go `GetExternalTimestamp` (registered by the domain / stale read).
// go-dep: github.com/pingcap/tidb/pkg/sessionctx/variable.GetExternalTimestamp L1
pub static GetExternalTimestamp: Callback<dyn Fn() -> Result<u64, Error> + Send + Sync> =
    RwLock::new(None);
/// Go `ValidateCloudStorageURI` (registered by lightning / BR).
// go-dep: github.com/pingcap/tidb/pkg/sessionctx/variable.ValidateCloudStorageURI L1
pub static ValidateCloudStorageURI: Callback<ValidateURIFn> = RwLock::new(None);
/// `func(ctx context.Context, uri string) error`.
pub type ValidateURIFn = dyn Fn(&str) -> Result<(), Error> + Send + Sync;
/// Go `SetGlobalResourceControl` (registered by the domain).
// go-dep: github.com/pingcap/tidb/pkg/sessionctx/variable.SetGlobalResourceControl L1
pub static SetGlobalResourceControl: Callback<dyn Fn(bool) + Send + Sync> = RwLock::new(None);
/// Go `SetLowResolutionTSOUpdateInterval` (registered by the domain; nil-checked).
// go-dep: github.com/pingcap/tidb/pkg/sessionctx/variable.SetLowResolutionTSOUpdateInterval L1
pub static SetLowResolutionTSOUpdateInterval: Callback<
    dyn Fn(std::time::Duration) -> Result<(), Error> + Send + Sync,
> = RwLock::new(None);
/// Go `ChangeSchemaCacheSize` (registered by the domain; nil-checked).
// go-dep: github.com/pingcap/tidb/pkg/sessionctx/variable.ChangeSchemaCacheSize L1
pub static ChangeSchemaCacheSize: Callback<dyn Fn(u64) -> Result<(), Error> + Send + Sync> =
    RwLock::new(None);
/// Go `ChangePDMetadataCircuitBreakerErrorRateThresholdRatio` (nil-checked).
// go-dep: github.com/pingcap/tidb/pkg/sessionctx/variable.ChangePDMetadataCircuitBreakerErrorRateThresholdRatio L1
pub static ChangePDMetadataCircuitBreakerErrorRateThresholdRatio: Callback<
    dyn Fn(u32) + Send + Sync,
> = RwLock::new(None);

/// The options of `stmtsummary.StmtSummaryByDigestMap` (statement summary v1; the summary
/// itself comes with D14.2): `optEnabled`, `optEnableInternalQuery`, `optRefreshInterval`,
/// `optHistorySize`, `optMaxStmtCount`, `optMaxSQLLength`.
// go-dep: github.com/pingcap/tidb/pkg/util/stmtsummary.stmtSummaryByDigestMap L2 (options only; the map is D14.2)
pub mod stmt_summary {
    use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, AtomicU32};

    /// `optEnabled`.
    pub static Enabled: AtomicBool = AtomicBool::new(false);
    /// `optEnableInternalQuery`.
    pub static EnableInternalQuery: AtomicBool = AtomicBool::new(false);
    /// `optRefreshInterval`.
    pub static RefreshInterval: AtomicI64 = AtomicI64::new(0);
    /// `optHistorySize`.
    pub static HistorySize: AtomicI32 = AtomicI32::new(0);
    /// `optMaxStmtCount`.
    pub static MaxStmtCount: AtomicU32 = AtomicU32::new(0);
    /// `optMaxSQLLength`.
    pub static MaxSQLLength: AtomicI32 = AtomicI32::new(0);
}

/// The callbacks above that no component has registered yet. Calling one panics like Go's
/// nil function call; Go's session recovers that panic into a client error, tidb-rs must not
/// reach it in production. A server start-up test asserts this list is empty once the
/// registering components exist (D05.2 acceptance item; Lucky's review of tidb-rs #66).
// glue: start-up check for the nil-callback panic path
pub fn unregistered_callbacks() -> Vec<&'static str> {
    let mut out = Vec::new();
    macro_rules! check {
        ($($c:ident),* $(,)?) => {$(
            if $c.read().is_none() {
                out.push(stringify!($c));
            }
        )*};
    }
    check!(
        SetMemQuotaAnalyze,
        GetMemQuotaAnalyze,
        SetStatsCacheCapacity,
        SetPDClientDynamicOption,
        SwitchMDL,
        SetExternalTimestamp,
        GetExternalTimestamp,
        ValidateCloudStorageURI,
        SetGlobalResourceControl,
        SetLowResolutionTSOUpdateInterval,
        ChangeSchemaCacheSize,
        ChangePDMetadataCircuitBreakerErrorRateThresholdRatio,
    );
    out
}
