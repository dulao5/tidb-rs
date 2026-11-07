//! The cgroup memory limit and usage read by `meminfo` (Go `pkg/util/cgroup`).
//!
//! `pkg/util/cgroup` is block X (not ported) in the tracker; `meminfo` needs three of its
//! functions. This is a reduced Rust version (glue), not a port:
//!
//! - `InContainer` is Go's check (`/proc/self/cgroup` mentions docker / kubepods /
//!   containerd, or the root of `/proc/self/mountinfo` is an overlay mount).
//! - The limit and usage files are Go's (v1: `memory.stat` `hierarchical_memory_limit`,
//!   `memory.usage_in_bytes`; v2: `memory.max` with `max` = `i64::MAX`, `memory.current`),
//!   but the controller is assumed to be mounted at `/sys/fs/cgroup` (v2) or
//!   `/sys/fs/cgroup/memory` (v1): Go parses `/proc/self/mountinfo` for the mount point.
//!
//! On other systems than Linux every function reports "no cgroup", as Go's does on darwin
//! (the `/proc` files do not exist).

// Path constants keep their Go names (mapping rule M-GONAME).
#![allow(non_upper_case_globals)]

use std::path::Path;
use std::path::PathBuf;

use tidb_base::Error;
use tidb_base::errors;

const procPathCGroup: &str = "/proc/self/cgroup";
const procPathMountInfo: &str = "/proc/self/mountinfo";

/// Whether the process runs in a container (Go `cgroup.InContainer`).
// glue: Go pkg/util/cgroup.InContainer (block X), same checks
pub fn in_container() -> bool {
    cfg!(target_os = "linux")
        && (in_container_at(procPathCGroup) || in_container_at(procPathMountInfo))
}

// glue: Go pkg/util/cgroup.inContainer (block X)
fn in_container_at(path: &str) -> bool {
    let Ok(v) = std::fs::read_to_string(path) else {
        return false;
    };
    if path == procPathCGroup
        && (v.contains("docker") || v.contains("kubepods") || v.contains("containerd"))
    {
        return true;
    }
    if path == procPathMountInfo {
        for line in v.split('\n') {
            let v: Vec<&str> = line.split(' ').collect();
            // The mount point of the root dir is on overlay: v[4] is the mount point, v[8]
            // the filesystem type (proc(5)).
            if v.len() > 8 && v[4] == "/" && v[8] == "overlay" {
                return true;
            }
        }
    }
    false
}

/// The memory controller's cgroup path and version from `/proc/self/cgroup`.
// glue: reduced Go detectControlPath / getCgroupDetails (block X)
fn memory_cgroup() -> Result<Option<(u8, PathBuf)>, Error> {
    let v = std::fs::read_to_string(procPathCGroup)
        .map_err(|e| errors::new(format!("open {procPathCGroup}: {e}")))?;
    let mut v2 = None;
    for line in v.lines() {
        let mut parts = line.splitn(3, ':');
        let (Some(_id), Some(ctrls), Some(path)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        if ctrls.split(',').any(|c| c == "memory") {
            return Ok(Some((1, PathBuf::from(path))));
        }
        if ctrls.is_empty() {
            v2 = Some((2, PathBuf::from(path)));
        }
    }
    Ok(v2)
}

// glue: Go readInt64Value (block X)
fn read_int64_value(root: &Path, filename: &str, cg_version: u8) -> Result<u64, Error> {
    let file_path = root.join(filename);
    let s = std::fs::read_to_string(&file_path).map_err(|e| {
        errors::new(format!(
            "can't read {filename} from cgroup v{cg_version}: {e}"
        ))
    })?;
    let trimmed = s.lines().next().unwrap_or("").trim();
    if trimmed == "max" {
        return Ok(i64::MAX as u64);
    }
    trimmed.parse::<u64>().map_err(|e| {
        errors::new(format!(
            "failed to parse value in {filename} from cgroup v{cg_version}: {e}"
        ))
    })
}

// glue: Go detectMemStatValue (block X)
fn detect_mem_stat_value(
    root: &Path,
    filename: &str,
    key: &str,
    cg_version: u8,
) -> Result<u64, Error> {
    let s = std::fs::read_to_string(root.join(filename)).map_err(|e| {
        errors::new(format!(
            "can't read file {filename} from cgroup v{cg_version}: {e}"
        ))
    })?;
    for line in s.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() != 2 || fields[0] != key {
            continue;
        }
        return fields[1].parse::<u64>().map_err(|e| {
            errors::new(format!(
                "can't read {key:?} memory stat from cgroup v{cg_version} in {filename}: {e}"
            ))
        });
    }
    Err(errors::new(format!(
        "failed to find expected memory stat {key:?} for cgroup v{cg_version} in {filename}"
    )))
}

/// The cgroup memory limit (Go `cgroup.GetMemoryLimit`); 0 without a memory controller.
// glue: reduced Go pkg/util/cgroup.GetMemoryLimit (block X)
pub fn get_memory_limit() -> Result<u64, Error> {
    match memory_cgroup()? {
        None => {
            tracing::warn!("no cgroup memory controller detected");
            Ok(0)
        }
        Some((1, _)) => detect_mem_stat_value(
            Path::new("/sys/fs/cgroup/memory"),
            "memory.stat",
            "hierarchical_memory_limit",
            1,
        ),
        Some((_, path)) => read_int64_value(&v2_root(&path), "memory.max", 2),
    }
}

/// The cgroup memory usage (Go `cgroup.GetMemoryUsage`); 0 without a memory controller.
// glue: reduced Go pkg/util/cgroup.GetMemoryUsage (block X)
pub fn get_memory_usage() -> Result<u64, Error> {
    match memory_cgroup()? {
        None => {
            tracing::warn!("no cgroup memory controller detected");
            Ok(0)
        }
        Some((1, _)) => read_int64_value(
            Path::new("/sys/fs/cgroup/memory"),
            "memory.usage_in_bytes",
            1,
        ),
        Some((_, path)) => read_int64_value(&v2_root(&path), "memory.current", 2),
    }
}

// glue: cgroup v2 directory of the process (mount point assumed at /sys/fs/cgroup)
fn v2_root(path: &Path) -> PathBuf {
    let rel = path.strip_prefix("/").unwrap_or(path);
    Path::new("/sys/fs/cgroup").join(rel)
}
