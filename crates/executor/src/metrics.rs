//! go-file: pkg/executor/metrics/metrics.go
//!
//! The executor's metric observers. Go binds them to the metric vectors of `pkg/metrics`
//! (`ExecPhaseDuration`, `SessionExecuteRunDuration`, ...), which `tidb-metrics` (D14.1) does
//! not have yet: the observers exist (the phase map and the callers are ported) and record
//! nothing until D14.1 provides the vectors.

// Go identifiers for the constants and observers (M-GONAME).
#![allow(non_upper_case_globals)]

use std::collections::HashMap;
use std::sync::LazyLock;

pub const PhaseBuildLocking: &str = "build:locking";
pub const PhaseOpenLocking: &str = "open:locking";
pub const PhaseNextLocking: &str = "next:locking";
pub const PhaseLockLocking: &str = "lock:locking";
pub const PhaseBuildFinal: &str = "build:final";
pub const PhaseOpenFinal: &str = "open:final";
pub const PhaseNextFinal: &str = "next:final";
pub const PhaseLockFinal: &str = "lock:final";
pub const PhaseCommitPrewrite: &str = "commit:prewrite";
pub const PhaseCommitCommit: &str = "commit:commit";
pub const PhaseCommitWaitCommitTS: &str = "commit:wait:commit-ts";
pub const PhaseCommitWaitLatestTS: &str = "commit:wait:latest-ts";
pub const PhaseCommitWaitLatch: &str = "commit:wait:local-latch";
pub const PhaseCommitWaitBinlog: &str = "commit:wait:prewrite-binlog";
pub const PhaseWriteResponse: &str = "write-response";

/// A `prometheus.Observer` of one labelled series: `(metric, labels)`. Observing is a no-op
/// until `tidb-metrics` has the vector (D14.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Observer {
    pub metric: &'static str,
    pub labels: Vec<&'static str>,
}

impl Observer {
    /// `vec.WithLabelValues(labels...)`.
    // glue: the series of a metric vector (prometheus WithLabelValues)
    fn with_label_values(metric: &'static str, labels: &[&'static str]) -> Observer {
        Observer {
            metric,
            labels: labels.to_vec(),
        }
    }

    /// `Observe(v)`.
    // glue: stub, blocked-by:D14.1 (the metric vectors of pkg/metrics)
    pub fn observe(&self, v: f64) {
        let _ = v;
    }
}

/// The observers `InitMetricsVars` binds that the ported code uses.
pub struct MetricsVars {
    pub session_execute_run_duration_internal: Observer,
    pub session_execute_run_duration_general: Observer,
    pub exec_unknown: Observer,
    pub exec_unknown_internal: Observer,
}

/// `init()`: the metric vars and the phase maps.
pub static METRICS: LazyLock<(MetricsVars, PhaseDurationObserverMaps)> = LazyLock::new(init);

// go: init L1
fn init() -> (MetricsVars, PhaseDurationObserverMaps) {
    let vars = init_metrics_vars();
    let maps = init_phase_duration_observer_map();
    (vars, maps)
}

/// InitMetricsVars init executor metrics vars.
///
/// Only the observers the ported code calls are bound; the others (cop process / wait time,
/// pessimistic DML, fair locking, executor counters, MPP coordinator, index lookup push-down)
/// come with their callers.
// go: InitMetricsVars L2 (binds the observers the ported code calls; the vectors are D14.1)
fn init_metrics_vars() -> MetricsVars {
    MetricsVars {
        session_execute_run_duration_internal: Observer::with_label_values(
            "SessionExecuteRunDuration",
            &["internal"],
        ),
        session_execute_run_duration_general: Observer::with_label_values(
            "SessionExecuteRunDuration",
            &["general"],
        ),
        exec_unknown: Observer::with_label_values("ExecPhaseDuration", &["unknown", "0"]),
        exec_unknown_internal: Observer::with_label_values("ExecPhaseDuration", &["unknown", "1"]),
    }
}

/// `PhaseDurationObserverMap` / `PhaseDurationObserverMapInternal`.
pub struct PhaseDurationObserverMaps {
    pub general: HashMap<&'static str, Observer>,
    pub internal: HashMap<&'static str, Observer>,
}

/// InitPhaseDurationObserverMap init observer map
// go: InitPhaseDurationObserverMap L1
fn init_phase_duration_observer_map() -> PhaseDurationObserverMaps {
    let phases = [
        PhaseBuildLocking,
        PhaseOpenLocking,
        PhaseNextLocking,
        PhaseLockLocking,
        PhaseBuildFinal,
        PhaseOpenFinal,
        PhaseNextFinal,
        PhaseLockFinal,
        PhaseCommitPrewrite,
        PhaseCommitCommit,
        PhaseCommitWaitCommitTS,
        PhaseCommitWaitLatestTS,
        PhaseCommitWaitLatch,
        PhaseCommitWaitBinlog,
        PhaseWriteResponse,
    ];
    let general = phases
        .iter()
        .map(|p| {
            (
                *p,
                Observer::with_label_values("ExecPhaseDuration", &[p, "0"]),
            )
        })
        .collect();
    let internal = phases
        .iter()
        .map(|p| {
            (
                *p,
                Observer::with_label_values("ExecPhaseDuration", &[p, "1"]),
            )
        })
        .collect();
    PhaseDurationObserverMaps { general, internal }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_maps() {
        let (vars, maps) = &*METRICS;
        assert_eq!(maps.general.len(), 15);
        assert_eq!(
            maps.internal[PhaseOpenFinal].labels,
            vec!["open:final", "1"]
        );
        assert_eq!(vars.exec_unknown.labels, vec!["unknown", "0"]);
    }
}
