//! go-file: pkg/util/cgroup/cgroup_memory.go
//! go-file: pkg/util/cgroup/cgroup.go (the path detection: controllerMatch,
//!   detectControlPath, getCgroupDetails, detectCgroupVersion, readFile, readInt64Value)
//! go-file: pkg/util/cgroup/cgroup_cpu_linux.go (InContainer, inContainer)
//! go-file: pkg/util/cgroup/cgroup_cpu_unsupport.go (InContainer)
//!
//! The memory part of `pkg/util/cgroup` (block D15.3): the memory controller's cgroup path
//! from `/proc/self/cgroup`, its mount point from `/proc/self/mountinfo`, and the limit,
//! usage and inactive-file usage files of cgroup v1 / v2 (hybrid: v1 first, v2 as the
//! fallback). The CPU part of the package stays X.
//!
//! Mapping:
//! - Go strings are bytes: paths and file contents are `&[u8]`, joined and cleaned by
//!   `gostd::path` (Go `filepath`), split by `gostd::bytes` (Go `bytes.Fields`,
//!   `bytes.TrimSpace`, `unicode.IsSpace`) and `gostd::bufio::scan_lines` (Go's default
//!   `bufio.Scanner`, including its 64 KiB line limit).
//! - `errors.Wrapf(err, f, ...)` is the message `f + ": " + err.Error()`; `os.Open` errors
//!   are Go's `*os.PathError` texts (`gostd::os`). `errors.Wrapf(nil, ...)` is nil, so an
//!   empty value file reads as 0 without an error, as in Go (`readInt64Value`).
//! - `log.Warn` is `tracing::warn!`.
//! - The `root` argument of the unexported Go functions ("/" in production, a fixture
//!   directory in tests) is kept: the `*_at` functions.
//! - Only `InContainer` has Go build tags (linux / !linux); the rest of the package builds
//!   on every OS and reads the `/proc` paths wherever it runs.

// Constants keep their Go names (mapping rule M-GONAME).
#![allow(non_upper_case_globals)]

use tidb_base::Error;
use tidb_base::errors;
use tidb_base::gostd::bufio::scan_lines;
use tidb_base::gostd::bytes::fields;
use tidb_base::gostd::bytes::trim_space;
use tidb_base::gostd::os::open_read_all;
use tidb_base::gostd::os::open_read_all_err;
use tidb_base::gostd::path::join_bytes;
use tidb_base::gostd::path::rel_bytes;
use tidb_base::gostd::strconv::parse_uint10;

/// Go `cgroup.Version`: the cgroup version.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Version {
    Unknown = 0,
    V1 = 1,
    V2 = 2,
}

// They are cgroup filename for different data
const cgroupV1MemStat: &str = "memory.stat";
const cgroupV2MemStat: &str = "memory.stat";
const cgroupV2MemLimit: &str = "memory.max";
const cgroupV1MemUsage: &str = "memory.usage_in_bytes";
const cgroupV2MemUsage: &str = "memory.current";

// {memory|cpu}.stat file keys
//
// key for # of bytes of file-backed memory on inactive LRU list in cgroupv1
const cgroupV1MemInactiveFileUsageStatKey: &str = "total_inactive_file";
// key for # of bytes of file-backed memory on inactive LRU list in cgroupv2
const cgroupV2MemInactiveFileUsageStatKey: &str = "inactive_file";
const cgroupV1MemLimitStatKey: &str = "hierarchical_memory_limit";

const procPathCGroup: &str = "/proc/self/cgroup";
const procPathMountInfo: &str = "/proc/self/mountinfo";

/// Go `math.MaxInt64` as the `uint64` that `readInt64Value` returns for `max`.
const MAX_INT64_U64: u64 = i64::MAX as u64;

/// `errors.Wrapf(cause, msg)`: `msg + ": " + cause`.
// glue: pingcap/errors.Wrapf message form (withMessage.Error)
fn wrap(msg: Vec<u8>, cause: &[u8]) -> Error {
    let mut m = msg;
    m.extend_from_slice(b": ");
    m.extend_from_slice(cause);
    errors::new(m)
}

// glue: byte-string concatenation
fn cat(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
}

/// GetMemoryLimit attempts to retrieve the cgroup memory limit for the current
/// process.
// go: GetMemoryLimit L1
pub fn get_memory_limit() -> Result<u64, Error> {
    get_cgroup_mem_limit_at(b"/").map(|(limit, _)| limit)
}

/// GetCgroupMemLimit attempts to retrieve the cgroup memory limit for the current
/// process, and return cgroup version too.
// go: GetCgroupMemLimit L1
pub fn get_cgroup_mem_limit() -> Result<(u64, Version), Error> {
    get_cgroup_mem_limit_at(b"/")
}

/// GetMemoryUsage attempts to retrieve the cgroup memory usage value (in bytes)
/// for the current process.
// go: GetMemoryUsage L1
pub fn get_memory_usage() -> Result<u64, Error> {
    get_cgroup_mem_usage_at(b"/")
}

/// GetMemoryInactiveFileUsage attempts to retrieve the cgroup memory usage value (in bytes)
/// for the current process.
// go: GetMemoryInactiveFileUsage L1
pub fn get_memory_inactive_file_usage() -> Result<u64, Error> {
    get_cgroup_mem_inactive_file_usage_at(b"/")
}

/// The memory controller's path and mount details under `root`, or `None` (after the Go
/// warning) when there is no memory controller.
// go-part: getCgroupMemLimit (the head shared by the three getCgroupMem* functions)
#[allow(clippy::type_complexity)]
fn memory_details(root: &[u8]) -> Result<Option<(Vec<u8>, Vec<Vec<u8>>, Vec<i32>)>, Error> {
    let path = detect_control_path(&join_bytes(&[root, procPathCGroup.as_bytes()]), "memory")?;
    if path.is_empty() {
        tracing::warn!("no cgroup memory controller detected");
        return Ok(None);
    }
    let (mount, ver) = get_cgroup_details(
        &join_bytes(&[root, procPathMountInfo.as_bytes()]),
        &path,
        "memory",
    )?;
    Ok(Some((path, mount, ver)))
}

// glue: fmt.Errorf("...%d", ver) with ver an []int
fn unknown_version(ver: &[i32]) -> Error {
    // fmt.Errorf("...: %d", ver) with ver an []int: "[1 2]".
    let items: Vec<String> = ver.iter().map(i32::to_string).collect();
    errors::new(format!(
        "detected unknown cgroup version index: [{}]",
        items.join(" ")
    ))
}

/// root is always "/" in the production. It will be changed for testing.
// go: getCgroupMemInactiveFileUsage L1
pub(crate) fn get_cgroup_mem_inactive_file_usage_at(root: &[u8]) -> Result<u64, Error> {
    let Some((path, mount, ver)) = memory_details(root)? else {
        return Ok(0);
    };
    if mount.len() == 2 {
        detect_mem_inactive_file_usage_in_v1(&join_bytes(&[root, &mount[0]])).or_else(|_| {
            detect_mem_inactive_file_usage_in_v2(&join_bytes(&[root, &mount[1], &path]))
        })
    } else {
        match ver[0] {
            1 => detect_mem_inactive_file_usage_in_v1(&join_bytes(&[root, &mount[0]])),
            2 => detect_mem_inactive_file_usage_in_v2(&join_bytes(&[root, &mount[0], &path])),
            _ => Err(unknown_version(&ver)),
        }
    }
}

/// getCgroupMemUsage reads the memory cgroup's current memory usage (in bytes).
// go: getCgroupMemUsage L1
pub(crate) fn get_cgroup_mem_usage_at(root: &[u8]) -> Result<u64, Error> {
    let Some((path, mount, ver)) = memory_details(root)? else {
        return Ok(0);
    };
    if ver.len() == 2 {
        // Go falls back to the v2 file under mount[0] (the v1 mount point), unlike the
        // limit and the inactive-file usage, which use mount[1].
        detect_mem_usage_in_v1(&join_bytes(&[root, &mount[0]]))
            .or_else(|_| detect_mem_usage_in_v2(&join_bytes(&[root, &mount[0], &path])))
    } else {
        match ver[0] {
            // cgroupv1
            1 => detect_mem_usage_in_v1(&join_bytes(&[root, &mount[0]])),
            // cgroupv2
            2 => detect_mem_usage_in_v2(&join_bytes(&[root, &mount[0], &path])),
            _ => Err(unknown_version(&ver)),
        }
    }
}

/// root is always "/" in the production. It will be changed for testing.
// go: getCgroupMemLimit L1
pub(crate) fn get_cgroup_mem_limit_at(root: &[u8]) -> Result<(u64, Version), Error> {
    // Go returns the version reached so far together with an error; callers only read it
    // when err == nil, so the Rust error carries no version.
    let Some((path, mount, ver)) = memory_details(root)? else {
        return Ok((0, Version::Unknown));
    };
    if ver.len() == 2 {
        match detect_mem_limit_in_v1(&join_bytes(&[root, &mount[0]])) {
            Ok(limit) => Ok((limit, Version::V1)),
            Err(_) => detect_mem_limit_in_v2(&join_bytes(&[root, &mount[1], &path]))
                .map(|limit| (limit, Version::V2)),
        }
    } else {
        match ver[0] {
            // cgroupv1
            1 => detect_mem_limit_in_v1(&join_bytes(&[root, &mount[0]]))
                .map(|limit| (limit, Version::V1)),
            // cgroupv2
            2 => detect_mem_limit_in_v2(&join_bytes(&[root, &mount[0], &path]))
                .map(|limit| (limit, Version::V2)),
            _ => Err(unknown_version(&ver)),
        }
    }
}

// go: detectMemLimitInV1 L1
fn detect_mem_limit_in_v1(c_root: &[u8]) -> Result<u64, Error> {
    detect_mem_stat_value(c_root, cgroupV1MemStat, cgroupV1MemLimitStatKey, 1)
}

// TODO(hawkingrei): this implementation was based on podman+criu environment.
// It may cover not all the cases when v2 becomes more widely used in container
// world.
// In K8S env, the value hold in memory.max is the memory limit defined in pod definition,
// the real memory limit should be the minimum value in the whole cgroup hierarchy
// as in cgroup V1, but cgroup V2 lacks the feature, see this patch for more details:
// https://lore.kernel.org/linux-kernel/ZctiM5RintD_D0Lt@host1.jankratochvil.net/T/
// So, in cgroup V2, the value better be adjusted by some factor, like 0.9, to
// avoid OOM. see taskexecutor/manager.go too.
// go: detectMemLimitInV2 L1
fn detect_mem_limit_in_v2(c_root: &[u8]) -> Result<u64, Error> {
    read_int64_value(c_root, cgroupV2MemLimit, 2)
}

// go: detectMemInactiveFileUsageInV1 L1
fn detect_mem_inactive_file_usage_in_v1(root: &[u8]) -> Result<u64, Error> {
    detect_mem_stat_value(
        root,
        cgroupV1MemStat,
        cgroupV1MemInactiveFileUsageStatKey,
        1,
    )
}

// go: detectMemInactiveFileUsageInV2 L1
fn detect_mem_inactive_file_usage_in_v2(root: &[u8]) -> Result<u64, Error> {
    detect_mem_stat_value(
        root,
        cgroupV2MemStat,
        cgroupV2MemInactiveFileUsageStatKey,
        2,
    )
}

// go: detectMemStatValue L1
pub(crate) fn detect_mem_stat_value(
    c_root: &[u8],
    filename: &str,
    key: &str,
    cg_version: i32,
) -> Result<u64, Error> {
    let stat_file_path = join_bytes(&[c_root, filename.as_bytes()]);
    let stat = open_read_all(&stat_file_path).map_err(|e| {
        wrap(
            format!("can't read file {filename} from cgroup v{cg_version}").into_bytes(),
            &e,
        )
    })?;
    for line in scan_lines(&stat) {
        let f = fields(line);
        if f.len() != 2 || f[0] != key.as_bytes() {
            continue;
        }
        let trimmed = trim_space(f[1]);
        return parse_uint10(trimmed).map_err(|e| {
            wrap(
                format!("can't read {key:?} memory stat from cgroup v{cg_version} in {filename}")
                    .into_bytes(),
                &e,
            )
        });
    }
    Err(errors::new(format!(
        "failed to find expected memory stat {key:?} for cgroup v{cg_version} in {filename}"
    )))
}

// go: detectMemUsageInV1 L1
fn detect_mem_usage_in_v1(c_root: &[u8]) -> Result<u64, Error> {
    read_int64_value(c_root, cgroupV1MemUsage, 1)
}

// TODO(hawkingrei): this implementation was based on podman+criu environment. It
// may cover not all the cases when v2 becomes more widely used in container
// world.
// go: detectMemUsageInV2 L1
fn detect_mem_usage_in_v2(c_root: &[u8]) -> Result<u64, Error> {
    read_int64_value(c_root, cgroupV2MemUsage, 2)
}

/// `os.Open` + `io.ReadAll`: the contents, and the read error if one ended them. Go's
/// `errors.Join(err, f.Close())` keeps the read error's text.
// go: readFile L1
pub(crate) fn read_file(filepath: &[u8]) -> (Vec<u8>, Option<Error>) {
    match open_read_all_err(filepath) {
        Err(open) => (Vec::new(), Some(errors::new(open))),
        Ok((data, read_err)) => (data, read_err.map(errors::new)),
    }
}

/// The field in /proc/self/cgroup and /proc/self/mountinfo may appear as "cpuacct,cpu" or "rw,cpuacct,cpu"
/// while the input controller is "cpu,cpuacct"
// go: controllerMatch L1
pub(crate) fn controller_match(field: &[u8], controller: &[u8]) -> bool {
    if field == controller {
        return true;
    }
    let fs: Vec<&[u8]> = field.split(|&b| b == b',').collect();
    if fs.len() < 2 {
        return false;
    }
    let cs: Vec<&[u8]> = controller.split(|&b| b == b',').collect();
    if fs.len() < cs.len() {
        return false;
    }
    cs.iter().all(|c| fs.contains(c))
}

/// The controller is defined via either type `memory` for cgroup v1 or via empty type for cgroup v2,
/// where the type is the second field in /proc/[pid]/cgroup file
// go: detectControlPath L1
pub(crate) fn detect_control_path(
    cgroup_file_path: &[u8],
    controller: &str,
) -> Result<Vec<u8>, Error> {
    let cgroup = open_read_all(cgroup_file_path).map_err(|e| {
        wrap(
            cat(&[
                format!("failed to read {controller} cgroup from cgroups file: ").as_bytes(),
                cgroup_file_path,
            ]),
            &e,
        )
    })?;
    let mut unified_path_if_found: Vec<u8> = Vec::new();
    for line in scan_lines(&cgroup) {
        let f: Vec<&[u8]> = line.split(|&b| b == b':').collect();
        if f.len() < 3 {
            // The lines should always have three fields, there's something fishy here.
            continue;
        }
        // First case if v2, second - v1. We give v2 the priority here.
        // There is also a `hybrid` mode when both  versions are enabled,
        // but no known container solutions support it.
        if f[0] == b"0" && f[1].is_empty() {
            unified_path_if_found = f[2].to_vec();
        } else if controller_match(f[1], controller.as_bytes()) {
            // In some case, the cgroup path contains `:`. We need to join them back.
            return Ok(f[2..].join(&b':'));
        }
    }
    Ok(unified_path_if_found)
}

/// See http://man7.org/linux/man-pages/man5/proc.5.html for `mountinfo` format.
// go: getCgroupDetails L1
pub(crate) fn get_cgroup_details(
    mount_info_path: &[u8],
    c_root: &[u8],
    controller: &str,
) -> Result<(Vec<Vec<u8>>, Vec<i32>), Error> {
    let info = open_read_all(mount_info_path).map_err(|e| {
        wrap(
            cat(&[b"failed to read mounts info from file: ", mount_info_path]),
            &e,
        )
    })?;
    let (mut found_ver1, mut found_ver2) = (false, false);
    let (mut mount_point_ver1, mut mount_point_ver2) = (Vec::new(), Vec::new());
    for line in scan_lines(&info) {
        let f = fields(line);
        if f.len() < 10 {
            continue;
        }
        if let Some(ver) = detect_cgroup_version(&f, controller.as_bytes()) {
            let mount_point = f[4];
            if ver == 2 {
                found_ver2 = true;
                mount_point_ver2 = mount_point.to_vec();
                continue;
            }
            // It is possible that the controller mount and the cgroup path are not the same (both are relative to the NS root).
            // So start with the mount and construct the relative path of the cgroup.
            let ns_relative_path = f[3];
            if !ns_relative_path.windows(2).any(|w| w == b"..") {
                // We don't expect to see err here ever but in case that it happens
                // the best action is to ignore the line and hope that the rest of the lines
                // will allow us to extract a valid path.
                if let Ok(rel_path) = rel_bytes(ns_relative_path, c_root) {
                    mount_point_ver1 = join_bytes(&[mount_point, &rel_path]);
                    found_ver1 = true;
                }
            }
        }
    }
    if found_ver1 && found_ver2 {
        return Ok((vec![mount_point_ver1, mount_point_ver2], vec![1, 2]));
    }
    if found_ver1 {
        return Ok((vec![mount_point_ver1], vec![1]));
    }
    if found_ver2 {
        return Ok((vec![mount_point_ver2], vec![2]));
    }
    Err(errors::new(
        "failed to detect cgroup root mount and version",
    ))
}

/// Return version of cgroup mount for memory controller if found
// go: detectCgroupVersion L1
pub(crate) fn detect_cgroup_version(f: &[&[u8]], controller: &[u8]) -> Option<i32> {
    if f.len() < 10 {
        return None;
    }
    // Due to strange format there can be optional fields in the middle of the set, starting
    // from the field #7. The end of the fields is marked with "-" field
    let mut pos = 6;
    while pos < f.len() {
        if f[pos] == b"-" {
            break;
        }
        pos += 1;
    }
    // No optional fields separator found or there is less than 3 fields after it which is wrong
    if (f.len() as isize - pos as isize - 1) < 3 {
        return None;
    }
    pos += 1;
    // Check for controller specifically in cgroup v1 (it is listed in super
    // options field), as the value can't be found if it is not enforced.
    if f[pos] == b"cgroup" && controller_match(f[pos + 2], controller) {
        return Some(1);
    } else if f[pos] == b"cgroup2" {
        return Some(2);
    }
    None
}

// go: readInt64Value L1
pub(crate) fn read_int64_value(root: &[u8], filename: &str, cg_version: i32) -> Result<u64, Error> {
    let file_path = join_bytes(&[root, filename.as_bytes()]);
    let file = open_read_all(&file_path).map_err(|e| {
        wrap(
            format!("can't read {filename} from cgroup v{cg_version}").into_bytes(),
            &e,
        )
    })?;
    let lines = scan_lines(&file);
    let Some(data) = lines.first() else {
        // Go: errors.Wrapf(err, "no value found ...") with err == nil (os.Open succeeded)
        // is nil, so an empty file reads as 0 without an error.
        return Ok(0);
    };
    let trimmed = trim_space(data);
    // cgroupv2 has certain control files that default to "max", so handle here.
    if trimmed == b"max" {
        return Ok(MAX_INT64_U64);
    }
    parse_uint10(trimmed).map_err(|e| {
        wrap(
            format!("failed to parse value in {filename} from cgroup v{cg_version}").into_bytes(),
            &e,
        )
    })
}

/// InContainer returns true if the process is running in a container.
// go: pkg/util/cgroup/cgroup_cpu_linux.go InContainer L1
#[cfg(target_os = "linux")]
pub fn in_container() -> bool {
    // for cgroup V1, check /proc/self/cgroup, for V2, check /proc/self/mountinfo
    in_container_at(procPathCGroup) || in_container_at(procPathMountInfo)
}

/// InContainer returns true if the process is running in a container.
// go: pkg/util/cgroup/cgroup_cpu_unsupport.go InContainer L1
#[cfg(not(target_os = "linux"))]
pub fn in_container() -> bool {
    false
}

// go: inContainer L1
#[cfg(target_os = "linux")]
fn in_container_at(path: &str) -> bool {
    let Ok(v) = std::fs::read(path) else {
        return false;
    };
    in_container_content(path, &v)
}

/// `inContainer` after `os.ReadFile(path)` succeeded with contents `v` (the checks depend
/// on which of the two `/proc` paths was read).
// glue: the body of Go inContainer, split from its os.ReadFile for tests
pub(crate) fn in_container_content(path: &str, v: &[u8]) -> bool {
    // For cgroup V1, check /proc/self/cgroup
    if path == procPathCGroup {
        let has = |s: &[u8]| v.windows(s.len()).any(|w| w == s);
        if has(b"docker") || has(b"kubepods") || has(b"containerd") {
            return true;
        }
    }
    // For cgroup V2, check /proc/self/mountinfo
    if path == procPathMountInfo {
        for line in v.split(|&b| b == b'\n') {
            let v: Vec<&[u8]> = line.split(|&b| b == b' ').collect();
            // check mount point of root dir is on overlay or not.
            // v[4] means `mount point`, v[8] means `filesystem type`.
            // see details from https://man7.org/linux/man-pages/man5/proc.5.html
            // TODO: enhance this check, as overlay is not the only storage driver for container.
            if v.len() > 8 && v[4] == b"/" && v[8] == b"overlay" {
                return true;
            }
        }
    }
    false
}

/// Entry points for the govec replay (`tests/govec_cgroup.rs`) into functions that are
/// unexported in Go. Not part of the tidb-memory API: hidden from the docs, and the
/// functions themselves stay `pub(crate)`.
#[doc(hidden)]
pub mod internal {
    use tidb_base::Error;

    pub use super::Version;

    /// The path constants, for tests of [`in_container_content`].
    pub const PROC_PATH_CGROUP: &str = super::procPathCGroup;
    /// See [`PROC_PATH_CGROUP`].
    pub const PROC_PATH_MOUNT_INFO: &str = super::procPathMountInfo;

    // glue: replay entry point
    pub fn get_cgroup_mem_inactive_file_usage_at(root: &[u8]) -> Result<u64, Error> {
        super::get_cgroup_mem_inactive_file_usage_at(root)
    }

    // glue: replay entry point
    pub fn get_cgroup_mem_usage_at(root: &[u8]) -> Result<u64, Error> {
        super::get_cgroup_mem_usage_at(root)
    }

    // glue: replay entry point
    pub fn get_cgroup_mem_limit_at(root: &[u8]) -> Result<(u64, Version), Error> {
        super::get_cgroup_mem_limit_at(root)
    }

    // glue: replay entry point
    pub fn detect_mem_stat_value(
        c_root: &[u8],
        filename: &str,
        key: &str,
        cg_version: i32,
    ) -> Result<u64, Error> {
        super::detect_mem_stat_value(c_root, filename, key, cg_version)
    }

    // glue: replay entry point
    pub fn read_file(filepath: &[u8]) -> (Vec<u8>, Option<Error>) {
        super::read_file(filepath)
    }

    // glue: replay entry point
    pub fn controller_match(field: &[u8], controller: &[u8]) -> bool {
        super::controller_match(field, controller)
    }

    // glue: replay entry point
    pub fn detect_control_path(
        cgroup_file_path: &[u8],
        controller: &str,
    ) -> Result<Vec<u8>, Error> {
        super::detect_control_path(cgroup_file_path, controller)
    }

    // glue: replay entry point
    pub fn get_cgroup_details(
        mount_info_path: &[u8],
        c_root: &[u8],
        controller: &str,
    ) -> Result<(Vec<Vec<u8>>, Vec<i32>), Error> {
        super::get_cgroup_details(mount_info_path, c_root, controller)
    }

    // glue: replay entry point
    pub fn detect_cgroup_version(f: &[&[u8]], controller: &[u8]) -> Option<i32> {
        super::detect_cgroup_version(f, controller)
    }

    // glue: replay entry point
    pub fn read_int64_value(root: &[u8], filename: &str, cg_version: i32) -> Result<u64, Error> {
        super::read_int64_value(root, filename, cg_version)
    }

    // glue: replay entry point
    pub fn in_container_content(path: &str, v: &[u8]) -> bool {
        super::in_container_content(path, v)
    }
}
