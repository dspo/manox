//! Captures build-time provenance (git commit SHA, rustc version) so the
//! binary can report exactly which revision and toolchain it was built from.

use std::path::PathBuf;

fn main() {
    // Resolve the real git dir — in a worktree `.git` is a file pointing to the
    // actual directory, so reading it gives us the correct `rerun-if-changed` targets.
    let (git_head, git_logs_head) = {
        let git_file = PathBuf::from("../../.git");
        let real_git_dir = if git_file.is_file() {
            std::fs::read_to_string(&git_file)
                .ok()
                .and_then(|s| s.strip_prefix("gitdir: ").map(|p| p.trim().to_string()))
                .map(PathBuf::from)
                .unwrap_or(git_file)
        } else {
            git_file
        };
        let head = real_git_dir.join("HEAD");
        let logs_head = real_git_dir.join("logs").join("HEAD");
        (head.display().to_string(), logs_head.display().to_string())
    };

    println!("cargo:rerun-if-changed={git_head}");
    println!("cargo:rerun-if-changed={git_logs_head}");

    if let Ok(output) = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        && output.status.success()
    {
        let sha = String::from_utf8_lossy(&output.stdout);
        println!("cargo:rustc-env=MANOX_COMMIT_SHA={}", sha.trim());
    } else {
        println!(
            "cargo:warning=git not available or not in a git repo; MANOX_COMMIT_SHA will be None"
        );
    }

    // Cargo exports RUSTC as the compiler path; `--version` yields the
    // toolchain string surfaced in the About window's structured block.
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());
    if let Ok(output) = std::process::Command::new(rustc).arg("--version").output()
        && output.status.success()
    {
        let version = String::from_utf8_lossy(&output.stdout);
        println!("cargo:rustc-env=RUSTC_VERSION={}", version.trim());
    } else {
        println!("cargo:warning=rustc version probe failed; RUSTC_VERSION will be None");
    }
}
