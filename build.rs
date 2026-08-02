use std::process::Command;

fn main() {
    let git_hash = Command::new("git")
        .args(["describe", "--always", "--dirty=+dev"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=GIT_HASH={git_hash}");
    println!("cargo:rerun-if-changed=.git/HEAD");
}
