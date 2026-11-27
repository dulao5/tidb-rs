//! Records the git commit and release version for server-info `git_hash` / `version` (D1 = A1).
use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
        .filter(|s| !s.is_empty())
}

fn env_override(name: &str) -> Option<String> {
    println!("cargo:rerun-if-env-changed={name}");
    std::env::var(name).ok().filter(|s| !s.is_empty())
}

fn main() {
    let hash = env_override("TIDB_RS_GIT_HASH")
        .or_else(|| git(&["rev-parse", "HEAD"]))
        .unwrap_or_else(|| "None".to_owned());
    let short_hash = hash.get(..7).unwrap_or(&hash);
    let release_version = env_override("TIDB_RS_RELEASE_VERSION")
        .unwrap_or_else(|| format!("v8.5.8+tidb-rs.{short_hash}"));
    println!("cargo:rustc-env=TIDB_RS_GIT_HASH={hash}");
    println!("cargo:rustc-env=TIDB_RS_RELEASE_VERSION={release_version}");
    // Rebuild when HEAD moves (a worktree has its own HEAD and reflog).
    if let Some(dir) = git(&["rev-parse", "--git-dir"]) {
        println!("cargo:rerun-if-changed={dir}/HEAD");
        println!("cargo:rerun-if-changed={dir}/logs/HEAD");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
