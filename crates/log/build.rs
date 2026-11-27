//! Compile-time build metadata injection (Go `Makefile.common` `-ldflags -X` equivalent).
//!
//! Injects:
//! - `TIDB_RS_GIT_HASH`
//! - `TIDB_RS_GIT_BRANCH`
//! - `TIDB_RS_BUILD_TS` (`YYYY-MM-DD HH:MM:SS` UTC)
//! - `TIDB_RS_RUST_VERSION` (first line of `rustc --version`)
//! - `TIDB_RS_RELEASE_VERSION` (default `v8.5.8+tidb-rs.<7-char-hash>` per ruling R35)
//!
//! Each variable can be overridden at build time by setting the corresponding environment variable.

use std::env;
use std::path::Path;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

const BASE_TIDB_RELEASE: &str = "v8.5.8";

fn main() {
    for var in [
        "TIDB_RS_GIT_HASH",
        "TIDB_RS_GIT_BRANCH",
        "TIDB_RS_BUILD_TS",
        "TIDB_RS_RUST_VERSION",
        "TIDB_RS_RELEASE_VERSION",
        "SOURCE_DATE_EPOCH",
    ] {
        println!("cargo:rerun-if-env-changed={var}");
    }
    println!("cargo:rerun-if-changed=build.rs");

    if let Some(git_dir) = git(&["rev-parse", "--git-dir"]) {
        let git_path = Path::new(&git_dir);
        let head = git_path.join("HEAD");
        if head.exists() {
            println!("cargo:rerun-if-changed={}", head.display());
            if let Ok(head_content) = std::fs::read_to_string(&head)
                && let Some(symref) = head_content.trim().strip_prefix("ref: ")
            {
                let common_dir = git(&["rev-parse", "--git-common-dir"]).unwrap_or(git_dir.clone());
                let ref_path = Path::new(&common_dir).join(symref.trim());
                if ref_path.exists() {
                    println!("cargo:rerun-if-changed={}", ref_path.display());
                }
                let packed = Path::new(&common_dir).join("packed-refs");
                if packed.exists() {
                    println!("cargo:rerun-if-changed={}", packed.display());
                }
            }
        }
        let logs_head = git_path.join("logs/HEAD");
        if logs_head.exists() {
            println!("cargo:rerun-if-changed={}", logs_head.display());
        }
    }

    let git_hash = env_non_empty("TIDB_RS_GIT_HASH")
        .or_else(|| git(&["rev-parse", "HEAD"]))
        .unwrap_or_else(|| "None".to_owned());

    let git_branch = env_non_empty("TIDB_RS_GIT_BRANCH")
        .or_else(|| git(&["rev-parse", "--abbrev-ref", "HEAD"]))
        .unwrap_or_else(|| "None".to_owned());

    let build_ts = env_non_empty("TIDB_RS_BUILD_TS").unwrap_or_else(utc_build_timestamp);

    let rust_version = env_non_empty("TIDB_RS_RUST_VERSION")
        .or_else(rustc_version)
        .unwrap_or_else(|| "unknown".to_owned());

    let release_version = env_non_empty("TIDB_RS_RELEASE_VERSION").unwrap_or_else(|| {
        let short = git_hash.get(..7).unwrap_or(&git_hash);
        format!("{BASE_TIDB_RELEASE}+tidb-rs.{short}")
    });

    println!("cargo:rustc-env=TIDB_RS_GIT_HASH={git_hash}");
    println!("cargo:rustc-env=TIDB_RS_GIT_BRANCH={git_branch}");
    println!("cargo:rustc-env=TIDB_RS_BUILD_TS={build_ts}");
    println!("cargo:rustc-env=TIDB_RS_RUST_VERSION={rust_version}");
    println!("cargo:rustc-env=TIDB_RS_RELEASE_VERSION={release_version}");
}

fn env_non_empty(name: &str) -> Option<String> {
    env::var(name).ok().filter(|v| !v.trim().is_empty())
}

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    let trimmed = s.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

fn rustc_version() -> Option<String> {
    let rustc = env::var("RUSTC").unwrap_or_else(|_| "rustc".to_owned());
    let out = Command::new(rustc).arg("--version").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    let line = s.lines().next()?.trim();
    if line.is_empty() {
        None
    } else {
        Some(line.to_owned())
    }
}

fn utc_build_timestamp() -> String {
    let secs = env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .ok()
                .map(|d| d.as_secs() as i64)
        });
    let Some(secs) = secs else {
        return "None".to_owned();
    };
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let hour = rem / 3_600;
    let minute = (rem % 3_600) / 60;
    let second = rem % 60;
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}")
}

/// Howard Hinnant's civil-from-days algorithm (days since 1970-01-01 -> (Y, M, D)).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    (year, m as u32, d as u32)
}
