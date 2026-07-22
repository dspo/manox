//! Captures the current git commit SHA at build time so the binary can
//! report exactly which revision it was built from.

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
}
