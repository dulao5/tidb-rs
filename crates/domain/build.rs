//! Records the git commit for the server-info `git_hash` / version suffix.
use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
        .filter(|s| !s.is_empty())
}

fn main() {
    let hash = git(&["rev-parse", "HEAD"]).unwrap_or_else(|| "None".to_owned());
    println!("cargo:rustc-env=TIDB_RS_GIT_HASH={hash}");
    // Rebuild when HEAD moves (a worktree has its own HEAD and reflog).
    if let Some(dir) = git(&["rev-parse", "--git-dir"]) {
        println!("cargo:rerun-if-changed={dir}/HEAD");
        println!("cargo:rerun-if-changed={dir}/logs/HEAD");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
