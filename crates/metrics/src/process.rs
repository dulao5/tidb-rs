//! `process_*` metrics with the names, types and help texts of Go's
//! `collectors.NewProcessCollector` (client_golang), which the TiDB dashboard's Server row
//! uses: uptime (`process_start_time_seconds`), CPU (`process_cpu_seconds_total`), memory
//! (`process_resident_memory_bytes`) and FDs (`process_open_fds`).
//!
//! The `prometheus` crate's own process collector is Linux-only, so values are read here, at
//! scrape time, on Linux and macOS:
//!
//! | metric | Linux | macOS |
//! |---|---|---|
//! | cpu seconds | `getrusage(RUSAGE_SELF)` user + system | same |
//! | resident / virtual bytes | `/proc/self/statm` × page size | `proc_pidinfo(PROC_PIDTASKINFO)` |
//! | open fds | entries of `/proc/self/fd` | entries of `/dev/fd` |
//! | max fds | `getrlimit(RLIMIT_NOFILE)` soft limit | same |
//! | start time | time [`ProcessCollector::new`] ran (process start + a few ms) | same |
//!
//! A value that cannot be read is left out of the scrape rather than reported as 0.

use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use prometheus::Counter;
use prometheus::Gauge;
use prometheus::Opts;
use prometheus::core::Collector;
use prometheus::core::Desc;
use prometheus::proto::MetricFamily;

/// Collects the `process_*` metrics of this process.
pub struct ProcessCollector {
    descs: Vec<Desc>,
    cpu_total: Counter,
    open_fds: Gauge,
    max_fds: Gauge,
    vsize: Gauge,
    rss: Gauge,
    start_time: Gauge,
}

impl ProcessCollector {
    /// A collector whose `process_start_time_seconds` is now.
    pub fn new() -> ProcessCollector {
        let cpu_total = Counter::with_opts(Opts::new(
            "process_cpu_seconds_total",
            "Total user and system CPU time spent in seconds.",
        ))
        .expect("valid metric");
        let open_fds = Gauge::with_opts(Opts::new(
            "process_open_fds",
            "Number of open file descriptors.",
        ))
        .expect("valid metric");
        let max_fds = Gauge::with_opts(Opts::new(
            "process_max_fds",
            "Maximum number of open file descriptors.",
        ))
        .expect("valid metric");
        let vsize = Gauge::with_opts(Opts::new(
            "process_virtual_memory_bytes",
            "Virtual memory size in bytes.",
        ))
        .expect("valid metric");
        let rss = Gauge::with_opts(Opts::new(
            "process_resident_memory_bytes",
            "Resident memory size in bytes.",
        ))
        .expect("valid metric");
        let start_time = Gauge::with_opts(Opts::new(
            "process_start_time_seconds",
            "Start time of the process since unix epoch in seconds.",
        ))
        .expect("valid metric");
        start_time.set(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0),
        );
        let mut descs = Vec::new();
        for c in [
            cpu_total.desc(),
            open_fds.desc(),
            max_fds.desc(),
            vsize.desc(),
            rss.desc(),
            start_time.desc(),
        ] {
            descs.extend(c.into_iter().cloned());
        }
        ProcessCollector {
            descs,
            cpu_total,
            open_fds,
            max_fds,
            vsize,
            rss,
            start_time,
        }
    }

    /// Unix seconds of `process_start_time_seconds`.
    pub fn start_time(&self) -> f64 {
        self.start_time.get()
    }
}

impl Default for ProcessCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl Collector for ProcessCollector {
    fn desc(&self) -> Vec<&Desc> {
        self.descs.iter().collect()
    }

    fn collect(&self) -> Vec<MetricFamily> {
        let mut out = Vec::with_capacity(6);
        if let Some(cpu) = cpu_seconds() {
            // A counter only goes up; `collect` may run concurrently, so add the delta.
            let delta = cpu - self.cpu_total.get();
            if delta > 0.0 {
                self.cpu_total.inc_by(delta);
            }
            out.extend(self.cpu_total.collect());
        }
        if let Some(n) = open_fds() {
            self.open_fds.set(n as f64);
            out.extend(self.open_fds.collect());
        }
        if let Some(n) = max_fds() {
            self.max_fds.set(n as f64);
            out.extend(self.max_fds.collect());
        }
        if let Some((rss, vsize)) = memory() {
            self.rss.set(rss as f64);
            self.vsize.set(vsize as f64);
            out.extend(self.rss.collect());
            out.extend(self.vsize.collect());
        }
        out.extend(self.start_time.collect());
        out
    }
}

/// User + system CPU seconds of the whole process.
fn cpu_seconds() -> Option<f64> {
    let usage = nix::sys::resource::getrusage(nix::sys::resource::UsageWho::RUSAGE_SELF).ok()?;
    let secs = |tv: nix::sys::time::TimeVal| tv.tv_sec() as f64 + tv.tv_usec() as f64 / 1e6;
    Some(secs(usage.user_time()) + secs(usage.system_time()))
}

/// Soft `RLIMIT_NOFILE` (Go reports the soft limit too).
fn max_fds() -> Option<u64> {
    let (soft, _hard) =
        nix::sys::resource::getrlimit(nix::sys::resource::Resource::RLIMIT_NOFILE).ok()?;
    Some(soft)
}

/// Open file descriptors: the entries of the per-process fd directory, minus the one the
/// directory listing itself holds open.
fn open_fds() -> Option<usize> {
    #[cfg(target_os = "linux")]
    const FD_DIR: &str = "/proc/self/fd";
    #[cfg(not(target_os = "linux"))]
    const FD_DIR: &str = "/dev/fd";
    let n = std::fs::read_dir(FD_DIR).ok()?.count();
    Some(n.saturating_sub(1))
}

/// `(resident bytes, virtual bytes)`.
#[cfg(target_os = "linux")]
fn memory() -> Option<(u64, u64)> {
    // statm: size resident shared text lib data dt, in pages.
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let mut fields = statm.split_whitespace();
    let size: u64 = fields.next()?.parse().ok()?;
    let resident: u64 = fields.next()?.parse().ok()?;
    let page = nix::unistd::sysconf(nix::unistd::SysconfVar::PAGE_SIZE)
        .ok()
        .flatten()
        .and_then(|p| u64::try_from(p).ok())
        .unwrap_or(4096);
    Some((resident * page, size * page))
}

#[cfg(target_os = "macos")]
fn memory() -> Option<(u64, u64)> {
    use libproc::proc_pid::pidinfo;
    use libproc::task_info::TaskInfo;
    let info = pidinfo::<TaskInfo>(std::process::id() as i32, 0).ok()?;
    Some((info.pti_resident_size, info.pti_virtual_size))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn memory() -> Option<(u64, u64)> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collects_every_process_metric() {
        let c = ProcessCollector::new();
        // Burn a little CPU so the counter is positive.
        let mut x = 0u64;
        for i in 0..2_000_000u64 {
            x = x.wrapping_mul(31).wrapping_add(i);
        }
        assert_ne!(x, 1);
        let families = c.collect();
        let names: Vec<&str> = families.iter().map(|f| f.get_name()).collect();
        for want in [
            "process_cpu_seconds_total",
            "process_open_fds",
            "process_max_fds",
            "process_resident_memory_bytes",
            "process_virtual_memory_bytes",
            "process_start_time_seconds",
        ] {
            assert!(names.contains(&want), "{want} missing from {names:?}");
        }
        let value = |name: &str| {
            let f = families.iter().find(|f| f.get_name() == name).unwrap();
            let m = &f.get_metric()[0];
            if f.get_field_type() == prometheus::proto::MetricType::COUNTER {
                m.get_counter().get_value()
            } else {
                m.get_gauge().get_value()
            }
        };
        assert!(value("process_cpu_seconds_total") > 0.0);
        assert!(value("process_open_fds") >= 3.0);
        assert!(value("process_max_fds") >= value("process_open_fds"));
        assert!(value("process_resident_memory_bytes") > 1e6);
        assert!(value("process_virtual_memory_bytes") >= value("process_resident_memory_bytes"));
        assert!(value("process_start_time_seconds") > 1.7e9);
    }
}
