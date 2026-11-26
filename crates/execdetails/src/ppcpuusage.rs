//! go-file: pkg/util/ppcpuusage/cpuusages.go
//!
//! Port of Go TiDB v8.5.8 `pkg/util/ppcpuusage/cpuusages.go` (block D00.3, 7 functions):
//! per-SQL TiDB/TiKV CPU time accounting (`CPUUsages` and `SQLCPUUsages`).

use std::sync::Mutex;
use std::time::Duration;

/// CPUUsages records tidb/tikv cpu usages.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CPUUsages {
    pub tidb_cpu_time: Duration,
    pub tikv_cpu_time: Duration,
}

impl CPUUsages {
    /// Reset resets all cpu times to 0.
    // go: CPUUsages.Reset L1
    pub fn reset(&mut self) {
        self.tikv_cpu_time = Duration::ZERO;
        self.tidb_cpu_time = Duration::ZERO;
    }
}

#[derive(Debug, Default)]
struct SQLCPUUsagesInner {
    sql_id: u64,
    cpu_usages: CPUUsages,
}

/// SQLCPUUsages is used to record sqlID and its cpu usages.
#[derive(Debug, Default)]
pub struct SQLCPUUsages {
    inner: Mutex<SQLCPUUsagesInner>,
}

impl SQLCPUUsages {
    /// SetCPUUsages sets cpu usages value.
    // go: SQLCPUUsages.SetCPUUsages L1 [M-LOCK]
    pub fn set_cpu_usages(&self, usage: CPUUsages) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.cpu_usages = usage;
    }

    /// MergeTidbCPUTime merges tidbCPU time into self when sqlID matches.
    /// Checks sqlID here, because tidb cpu time can only be collected by profiler now, and
    /// updated in concurrent goroutines.
    // go: SQLCPUUsages.MergeTidbCPUTime L1 [M-LOCK]
    pub fn merge_tidb_cpu_time(&self, sql_id: u64, d: Duration) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.sql_id == sql_id {
            inner.cpu_usages.tidb_cpu_time += d;
        }
    }

    /// MergeTikvCPUTime merges tikvCPU time into self.
    /// Doesn't need to check sqlID here, because tikv cpu time is updated in executors now.
    // go: SQLCPUUsages.MergeTikvCPUTime L1 [M-LOCK]
    pub fn merge_tikv_cpu_time(&self, d: Duration) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.cpu_usages.tikv_cpu_time += d;
    }

    /// GetCPUUsages returns tidbCPU, tikvCPU time.
    // go: SQLCPUUsages.GetCPUUsages L1 [M-LOCK]
    pub fn get_cpu_usages(&self) -> CPUUsages {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.cpu_usages
    }

    /// AllocNewSQLID alloc new ID, will restart from 0 when exceeds uint64 max limit.
    // go: SQLCPUUsages.AllocNewSQLID L1 [M-LOCK]
    pub fn alloc_new_sql_id(&self) -> u64 {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.sql_id = inner.sql_id.wrapping_add(1);
        inner.sql_id
    }

    /// ResetCPUTimes resets tidb/tikv cpu times to 0.
    // go: SQLCPUUsages.ResetCPUTimes L1 [M-LOCK]
    pub fn reset_cpu_times(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.cpu_usages.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_usages_lifecycle() {
        let u = SQLCPUUsages::default();
        assert_eq!(u.get_cpu_usages(), CPUUsages::default());

        let id1 = u.alloc_new_sql_id();
        assert_eq!(id1, 1);
        u.merge_tidb_cpu_time(id1, Duration::from_millis(10));
        u.merge_tidb_cpu_time(id1 + 1, Duration::from_millis(99)); // wrong sql_id ignored
        u.merge_tikv_cpu_time(Duration::from_millis(25));
        assert_eq!(
            u.get_cpu_usages(),
            CPUUsages {
                tidb_cpu_time: Duration::from_millis(10),
                tikv_cpu_time: Duration::from_millis(25),
            }
        );

        u.set_cpu_usages(CPUUsages {
            tidb_cpu_time: Duration::from_secs(1),
            tikv_cpu_time: Duration::from_secs(2),
        });
        assert_eq!(u.get_cpu_usages().tidb_cpu_time, Duration::from_secs(1));
        assert_eq!(u.get_cpu_usages().tikv_cpu_time, Duration::from_secs(2));

        u.reset_cpu_times();
        assert_eq!(u.get_cpu_usages(), CPUUsages::default());
    }
}
