//! go-file: pkg/sessionctx/variable/sysvar.go
//!
//! The GetGlobal / SetGlobal hooks of the five `tidb_gc_*` variables (D05.2 PR C, T10).
//! They read and write `mysql.tidb` through the session's `GlobalVarAccessor`
//! (`GetTiDBTableValue` / `SetTiDBTableValue`): the GC worker of a TiDB older than 5.0 reads
//! its settings from there. Registered in `session_hooks.rs`.

use tidb_base::Error;

use super::session::SessionVars;
use super::variable::{Off, On};
use super::varsutil::{get_tidb_table_value, set_tidb_table_value};

// go-part: defaultSysVars (sysvar.go:L979 tidb_gc_enable GetGlobal closure) L1
pub(super) fn gg_tidb_gc_enable(s: &mut SessionVars) -> Result<String, Error> {
    get_tidb_table_value(s, "tikv_gc_enable", On)
}

// go-part: defaultSysVars (sysvar.go:L981 tidb_gc_enable SetGlobal closure) L1
pub(super) fn sg_tidb_gc_enable(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    set_tidb_table_value(s, "tikv_gc_enable", val, "Current GC enable status")
}

// go-part: defaultSysVars (sysvar.go:L984 tidb_gc_run_interval GetGlobal closure) L1
pub(super) fn gg_tidb_gc_run_interval(s: &mut SessionVars) -> Result<String, Error> {
    get_tidb_table_value(s, "tikv_gc_run_interval", "10m0s")
}

// go-part: defaultSysVars (sysvar.go:L986 tidb_gc_run_interval SetGlobal closure) L1
pub(super) fn sg_tidb_gc_run_interval(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    set_tidb_table_value(
        s,
        "tikv_gc_run_interval",
        val,
        "GC run interval, at least 10m, in Go format.",
    )
}

// go-part: defaultSysVars (sysvar.go:L989 tidb_gc_life_time GetGlobal closure) L1
pub(super) fn gg_tidb_gc_life_time(s: &mut SessionVars) -> Result<String, Error> {
    get_tidb_table_value(s, "tikv_gc_life_time", "10m0s")
}

// go-part: defaultSysVars (sysvar.go:L991 tidb_gc_life_time SetGlobal closure) L1
pub(super) fn sg_tidb_gc_life_time(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    set_tidb_table_value(
        s,
        "tikv_gc_life_time",
        val,
        "All versions within life time will not be collected by GC, at least 10m, in Go format.",
    )
}

// go-part: defaultSysVars (sysvar.go:L994 tidb_gc_concurrency GetGlobal closure) L1
pub(super) fn gg_tidb_gc_concurrency(s: &mut SessionVars) -> Result<String, Error> {
    let auto_concurrency_val = get_tidb_table_value(s, "tikv_gc_auto_concurrency", On);
    if matches!(auto_concurrency_val.as_deref(), Ok(v) if v == On) {
        return Ok("-1".to_owned()); // convention for "AUTO"
    }
    get_tidb_table_value(s, "tikv_gc_concurrency", "-1")
}

// go-part: defaultSysVars (sysvar.go:L1000 tidb_gc_concurrency SetGlobal closure) L1
pub(super) fn sg_tidb_gc_concurrency(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    let mut auto_concurrency = Off;
    if val == "-1" {
        auto_concurrency = On;
    }
    // Update both autoconcurrency and concurrency.
    set_tidb_table_value(
        s,
        "tikv_gc_auto_concurrency",
        auto_concurrency,
        "Let TiDB pick the concurrency automatically. If set false, tikv_gc_concurrency will be used",
    )?;
    set_tidb_table_value(
        s,
        "tikv_gc_concurrency",
        val,
        "How many goroutines used to do GC parallel, [1, 256], default 2",
    )
}

// go-part: defaultSysVars (sysvar.go:L1011 tidb_gc_scan_lock_mode GetGlobal closure) L1
pub(super) fn gg_tidb_gc_scan_lock_mode(s: &mut SessionVars) -> Result<String, Error> {
    get_tidb_table_value(s, "tikv_gc_scan_lock_mode", "LEGACY")
}

// go-part: defaultSysVars (sysvar.go:L1013 tidb_gc_scan_lock_mode SetGlobal closure) L1
pub(super) fn sg_tidb_gc_scan_lock_mode(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    set_tidb_table_value(
        s,
        "tikv_gc_scan_lock_mode",
        val,
        "Mode of scanning locks, \"physical\" or \"legacy\"",
    )
}
