//! Captures the current git commit SHA at build time so the binary can
//! report exactly which revision it was built from.

fn main() {
    println!("cargo:rerun-if-changed=../../.git/logs/HEAD");
    println!("cargo:rerun-if-changed=../../.git/HEAD");

    if let Ok(output) = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        && output.status.success()
    {
        let sha = String::from_utf8_lossy(&output.stdout);
        println!("cargo:rustc-env=MANOX_COMMIT_SHA={}", sha.trim());
    }
}
