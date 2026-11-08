//! go-file: pkg/util/memory/meminfo.go
//!
//! System and instance memory. Go's `MemTotal` / `MemUsed` are package-level function
//! variables chosen in `init()` (cgroup or physical) and switched by `InitMemoryHook`; here
//! they are function pointers behind a lock, set up on first use ([`mem_total`] /
//! [`mem_used`]) and by [`init_memory_hook`].
//!
//! The physical numbers are gopsutil's `mem.VirtualMemory()` (v3.24.5), ported for the two
//! systems tidb-rs runs on: Linux reads `/proc/meminfo`
//! (`Used = Total - Free - Buffers - Cached - SReclaimable`), macOS runs `sysctl -n
//! hw.memsize` and `vm_stat` (`Used = Total - Free - Inactive`) — gopsutil's no-cgo path, so
//! the crate needs no `unsafe`. The cgroup numbers come from [`crate::cgroup`] (the memory part
//! of Go's `pkg/util/cgroup`, block D15.3). The `GetMemTotalError` failpoint and
//! `sysutil.RegisterGetMemoryCapacity` (the `INFORMATION_SCHEMA` cluster hardware tables,
//! block X) are not ported.

// Package-level names keep their Go names (mapping rule M-GONAME).
#![allow(non_upper_case_globals)]

use std::sync::LazyLock;
use std::time::Duration;
use std::time::Instant;

use parking_lot::RwLock;
use tidb_base::Error;
use tidb_base::errors;

use crate::cgroup;
use crate::memstats::read_mem_stats;

/// Go `func() (uint64, error)`.
pub type MemFn = fn() -> Result<u64, Error>;

/// Go `memInfoCache`.
#[derive(Debug, Default)]
struct MemInfoCache {
    /// `(mem, updateTime)`; `None` is Go's zero `time.Time` (never set).
    mu: RwLock<(u64, Option<Instant>)>,
}

impl MemInfoCache {
    // go: memInfoCache.get L1
    fn get(&self) -> (u64, Option<Instant>) {
        *self.mu.read()
    }

    // go: memInfoCache.set L1
    fn set(&self, memo: u64, t: Instant) {
        *self.mu.write() = (memo, Some(t));
    }
}

// glue: Go `time.Since(t) < d` with t possibly the zero time
fn fresh(t: Option<Instant>, d: Duration) -> bool {
    t.is_some_and(|t| t.elapsed() < d)
}

struct MemHooks {
    mem_total: MemFn,
    mem_used: MemFn,
}

static memLimit: LazyLock<MemInfoCache> = LazyLock::new(MemInfoCache::default);
static memUsage: LazyLock<MemInfoCache> = LazyLock::new(MemInfoCache::default);
static serverMemUsage: LazyLock<MemInfoCache> = LazyLock::new(MemInfoCache::default);

static HOOKS: LazyLock<RwLock<MemHooks>> = LazyLock::new(init);

/// Go `init()`: chooses the cgroup or the physical hooks, then reads both once (`terror.MustNil`
/// panics on error). Runs on first use.
// go: init L1
fn init() -> RwLock<MemHooks> {
    let hooks = if cgroup::in_container() {
        MemHooks {
            mem_total: mem_total_cgroup,
            mem_used: mem_used_cgroup,
        }
    } else {
        MemHooks {
            mem_total: mem_total_normal,
            mem_used: mem_used_normal,
        }
    };
    if let Err(e) = (hooks.mem_total)() {
        panic!("{e}");
    }
    if let Err(e) = (hooks.mem_used)() {
        panic!("{e}");
    }
    RwLock::new(hooks)
}

/// The total memory limit of the instance (Go `MemTotal()`).
// glue: call of the Go function variable MemTotal
pub fn mem_total() -> Result<u64, Error> {
    let f = HOOKS.read().mem_total;
    f()
}

/// The memory used by the instance's system or cgroup (Go `MemUsed()`).
// glue: call of the Go function variable MemUsed
pub fn mem_used() -> Result<u64, Error> {
    let f = HOOKS.read().mem_used;
    f()
}

/// Replaces the `MemTotal` / `MemUsed` hooks (Go assigns the variables; tests do too).
// glue: assignment of the Go function variables
pub fn set_mem_hooks(total: MemFn, used: MemFn) {
    let mut h = HOOKS.write();
    h.mem_total = total;
    h.mem_used = used;
}

/// The total memory, 0 on error.
// go: GetMemTotalIgnoreErr L1
pub fn get_mem_total_ignore_err() -> u64 {
    mem_total().unwrap_or(0)
}

/// The total memory of the system, cached for 60 s.
// go: MemTotalNormal L1
pub fn mem_total_normal() -> Result<u64, Error> {
    let (total, t) = memLimit.get();
    if fresh(t, Duration::from_secs(60)) {
        return Ok(total);
    }
    mem_total_normal_uncached()
}

// go: memTotalNormal L1
fn mem_total_normal_uncached() -> Result<u64, Error> {
    let v = virtual_memory()?;
    memLimit.set(v.total, Instant::now());
    Ok(v.total)
}

/// The memory used by the system, cached for 500 ms.
// go: MemUsedNormal L1
pub fn mem_used_normal() -> Result<u64, Error> {
    let (used, t) = memUsage.get();
    if fresh(t, Duration::from_millis(500)) {
        return Ok(used);
    }
    let v = virtual_memory()?;
    memUsage.set(v.used, Instant::now());
    Ok(v.used)
}

/// The memory limit of the cgroup (at most the physical total), cached for 60 s.
// go: MemTotalCGroup L1
pub fn mem_total_cgroup() -> Result<u64, Error> {
    let (memo, t) = memLimit.get();
    if fresh(t, Duration::from_secs(60)) {
        return Ok(memo);
    }
    let memo = cgroup::get_memory_limit()?;
    let v = virtual_memory()?;
    let memo = v.total.min(memo);
    memLimit.set(memo, Instant::now());
    Ok(memo)
}

/// The memory used by the cgroup (at most the physical used), cached for 500 ms.
// go: MemUsedCGroup L1
pub fn mem_used_cgroup() -> Result<u64, Error> {
    let (memo, t) = memUsage.get();
    if fresh(t, Duration::from_millis(500)) {
        return Ok(memo);
    }
    let memo = cgroup::get_memory_usage()?;
    let v = virtual_memory()?;
    let memo = v.used.min(memo);
    memUsage.set(memo, Instant::now());
    Ok(memo)
}

/// Switches to the cgroup hooks when the cgroup limit is below the physical memory.
// go: InitMemoryHook L1
pub fn init_memory_hook() {
    if cgroup::in_container() {
        tracing::info!("use cgroup memory hook because TiDB is in the container");
        return;
    }
    let Ok(cgroup_value) = cgroup::get_memory_limit() else {
        return;
    };
    let Ok(physical_value) = mem_total_normal_uncached() else {
        return;
    };
    if physical_value > cgroup_value && cgroup_value != 0 {
        set_mem_hooks(mem_total_cgroup, mem_used_cgroup);
        tracing::info!(
            cgroupMemorySize = cgroup_value as i64,
            physicalMemorySize = physical_value as i64,
            "use cgroup memory hook"
        );
    } else {
        tracing::info!(
            cgroupMemorySize = cgroup_value as i64,
            physicalMemorySize = physical_value as i64,
            "use physical memory hook"
        );
    }
    if let Err(e) = mem_total() {
        panic!("{e}");
    }
    if let Err(e) = mem_used() {
        panic!("{e}");
    }
}

/// The memory used by this instance's heap (Go: `runtime.MemStats.HeapAlloc`; here jemalloc
/// `stats.allocated`, see [`crate::memstats`]), cached for 500 ms.
// go: InstanceMemUsed L1
pub fn instance_mem_used() -> Result<u64, Error> {
    let (used, t) = serverMemUsage.get();
    if fresh(t, Duration::from_millis(500)) {
        return Ok(used);
    }
    let memory_usage = read_mem_stats().heap_alloc;
    serverMemUsage.set(memory_usage, Instant::now());
    Ok(memory_usage)
}

/// The fields of gopsutil's `mem.VirtualMemoryStat` that TiDB reads.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VirtualMemoryStat {
    /// Total amount of RAM.
    pub total: u64,
    /// RAM used by programs (see the module docs for the per-system formula).
    pub used: u64,
}

/// gopsutil `mem.VirtualMemory()`.
// go-dep: github.com/shirou/gopsutil/v3@v3.24.5.mem.VirtualMemory L2 (Linux /proc/meminfo; macOS through the no-cgo vm_stat path)
pub fn virtual_memory() -> Result<VirtualMemoryStat, Error> {
    #[cfg(target_os = "linux")]
    {
        let s = std::fs::read_to_string("/proc/meminfo")
            .map_err(|e| errors::new(format!("open /proc/meminfo: {e}")))?;
        parse_meminfo(&s)
    }
    #[cfg(target_os = "macos")]
    {
        let total = command_output("sysctl", &["-n", "hw.memsize"])?;
        let total: u64 = total
            .trim()
            .parse()
            .map_err(|e| errors::new(format!("sysctl hw.memsize: {e}")))?;
        let page_size = nix::unistd::sysconf(nix::unistd::SysconfVar::PAGE_SIZE)
            .ok()
            .flatten()
            .unwrap_or(4096) as u64;
        let out = command_output("vm_stat", &[])?;
        let (free, inactive) = parse_vm_stat(&out, page_size)?;
        // gopsutil darwin: Available = Free + Inactive; Used = Total - Available (uint64).
        Ok(VirtualMemoryStat {
            total,
            used: total.wrapping_sub(free.wrapping_add(inactive)),
        })
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        Err(errors::new("not implemented yet"))
    }
}

/// gopsutil `fillFromMeminfoWithContext` (the fields `Used` needs).
// go-dep: github.com/shirou/gopsutil/v3@v3.24.5.mem.fillFromMeminfoWithContext L2 (only Total, Free, Buffers, Cached, SReclaimable)
pub fn parse_meminfo(s: &str) -> Result<VirtualMemoryStat, Error> {
    let (mut total, mut free, mut buffers, mut cached, mut sreclaimable) =
        (0u64, 0u64, 0u64, 0u64, 0u64);
    for line in s.split('\n') {
        let fields: Vec<&str> = line.split(':').collect();
        if fields.len() != 2 {
            continue;
        }
        let key = fields[0].trim();
        let value = fields[1].trim().replace(" kB", "");
        let slot = match key {
            "MemTotal" => &mut total,
            "MemFree" => &mut free,
            "Buffers" => &mut buffers,
            "Cached" => &mut cached,
            "SReclaimable" => &mut sreclaimable,
            _ => continue,
        };
        let t: u64 = value.parse().map_err(|_| {
            errors::new(format!(
                "strconv.ParseUint: parsing {value:?}: invalid syntax"
            ))
        })?;
        *slot = t * 1024;
    }
    let cached = cached + sreclaimable;
    Ok(VirtualMemoryStat {
        total,
        used: total
            .wrapping_sub(free)
            .wrapping_sub(buffers)
            .wrapping_sub(cached),
    })
}

/// gopsutil `parseVMStat`: `(free, inactive)` bytes.
// go-dep: github.com/shirou/gopsutil/v3@v3.24.5.mem.parseVMStat L2 (only Pages free / inactive)
pub fn parse_vm_stat(out: &str, page_size: u64) -> Result<(u64, u64), Error> {
    let (mut free, mut inactive) = (0u64, 0u64);
    let mut err = None;
    for line in out.split('\n') {
        let fields: Vec<&str> = line.split(':').collect();
        if fields.len() < 2 {
            continue;
        }
        let key = fields[0].trim();
        let value = fields[1].trim_matches(|c| c == ' ' || c == '.');
        let slot = match key {
            "Pages free" => &mut free,
            "Pages inactive" => &mut inactive,
            _ => continue,
        };
        match value.parse::<u64>() {
            Ok(v) => *slot = v * page_size,
            Err(_) => {
                err = Some(errors::new(format!(
                    "strconv.ParseUint: parsing {value:?}: invalid syntax"
                )))
            }
        }
    }
    match err {
        Some(e) => Err(e),
        None => Ok((free, inactive)),
    }
}

#[cfg(target_os = "macos")]
// glue: gopsutil invoke.Command (runs a system command and returns its stdout)
fn command_output(cmd: &str, args: &[&str]) -> Result<String, Error> {
    let out = std::process::Command::new(cmd)
        .args(args)
        .output()
        .map_err(|e| errors::new(format!("{cmd}: {e}")))?;
    if !out.status.success() {
        return Err(errors::new(format!("{cmd}: {}", out.status)));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meminfo_used_is_gopsutils() {
        let s = "MemTotal:       16000000 kB\nMemFree:         1000000 kB\nMemAvailable:    9000000 kB\nBuffers:          500000 kB\nCached:          4000000 kB\nSReclaimable:     200000 kB\n";
        let v = parse_meminfo(s).unwrap();
        assert_eq!(v.total, 16000000 * 1024);
        assert_eq!(
            v.used,
            (16000000 - 1000000 - 500000 - 4000000 - 200000) * 1024
        );
    }

    #[test]
    fn vm_stat_free_inactive() {
        let s = "Mach Virtual Memory Statistics: (page size of 16384 bytes)\nPages free:                               10.\nPages active:                            300.\nPages inactive:                           20.\n";
        assert_eq!(parse_vm_stat(s, 16384).unwrap(), (10 * 16384, 20 * 16384));
    }

    #[test]
    fn system_memory_is_readable() {
        let total = mem_total().unwrap();
        let used = mem_used().unwrap();
        assert!(total > 0);
        assert!(used > 0 && used <= total);
        assert_eq!(get_mem_total_ignore_err(), total);
        // Cached: the second read is the same.
        assert_eq!(mem_total_normal().unwrap(), mem_total_normal().unwrap());
        init_memory_hook();
    }
}
